//! Detect the sub-agents a live Claude Code session is currently running.
//!
//! When Claude delegates work via the Agent/Task tool it spawns a
//! sub-agent whose transcript is written one level deeper than the
//! canonical session file, beside a small metadata sidecar:
//!
//! ```text
//! <project>/<sessionId>.jsonl                            ← parent (main agent)
//! <project>/<sessionId>/subagents/agent-<agentId>.jsonl       ← sub-agent transcript
//! <project>/<sessionId>/subagents/agent-<agentId>.meta.json   ← sub-agent metadata
//! ```
//!
//! The `.meta.json` sidecar carries `{agentType, description, toolUseId}`
//! — the friendly label, plus the `toolUseId` of the launching tool call.
//! That id is the key to a clean liveness test:
//!
//!   * a delegation is **finished** the moment the *parent* transcript
//!     records a `tool_result` for its `toolUseId`. This fires on normal
//!     return *and* on interrupt/rejection (an `is_error` result), so a
//!     stopped sub-agent disappears on the very next scan instead of
//!     lingering. The one exception is the `async_launched` ack a
//!     background agent gets at launch — that is not terminal and is
//!     excluded.
//!   * sub-agent transcripts accumulate for the whole life of the
//!     session, so a cheap mtime [`FRESH_WINDOW`] gate runs first: a
//!     session with hundreds of past delegations only ever parses the
//!     handful written recently. It also retires background agents (whose
//!     completion the parent doesn't record) and transcripts orphaned by
//!     a crash.
//!
//! The parent transcript is read only when at least one fresh sub-agent
//! file exists.
//!
//! A `Workflow` run — Claude orchestrating many agents from a script via
//! `agent()`/`parallel()`/`pipeline()` — fans out through a second,
//! parallel layout instead of the parent transcript's tool-call loop:
//!
//! ```text
//! <project>/<sessionId>/workflows/<runId>.json                          ← run summary (name, phases, live progress)
//! <project>/<sessionId>/subagents/workflows/<runId>/agent-<agentId>.jsonl       ← per-agent transcript (same shape as above)
//! <project>/<sessionId>/subagents/workflows/<runId>/agent-<agentId>.meta.json   ← sidecar: {agentType, spawnDepth} — no toolUseId
//! <project>/<sessionId>/subagents/workflows/<runId>/journal.jsonl               ← one {type: started|result, agentId} line per agent event
//! ```
//!
//! There is no launching tool call to resolve, so liveness is read from
//! `journal.jsonl` instead of the parent: an agent is finished once it has
//! any journal record other than `started`. The same [`FRESH_WINDOW`] mtime
//! gate bounds the IO, and the run summary supplies the concise per-agent
//! `label` (e.g. `verify:resolve_asset_names drops the combined query`)
//! that the sidecar — missing a `description` field here — can't.

use crate::live_session::{self, Activity, TailKind};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Max age of a sub-agent transcript's last write for it to still count
/// as running. Completion is detected authoritatively from the parent's
/// `tool_result` (see module docs), so this window's only jobs are to
/// bound the per-tick file IO and to retire agents that never get a
/// terminal record (background agents, crash orphans).
const FRESH_WINDOW: Duration = Duration::from_secs(90);

/// One sub-agent a session is actively running. Rendered as an indented
/// child row under its parent session in the all-sessions view.
#[derive(Clone, Debug, Serialize)]
pub struct SubAgent {
    pub agent_id: String,
    /// The sub-agent's own transcript (`…/subagents/agent-<id>.jsonl`).
    /// Lets the TUI open the agent in the session-detail view exactly
    /// like a top-level session.
    pub transcript_path: PathBuf,
    /// Agent kind from the sidecar (`Explore`, `general-purpose`, …).
    /// `None` only when the sidecar is missing (older Claude Code).
    pub agent_type: Option<String>,
    /// Short task label — the sidecar's `description`, falling back to the
    /// first non-empty line of the sub-agent's own prompt.
    pub description: String,
    /// Model the sub-agent is actually running on (often differs from the
    /// main agent — e.g. an Explore agent on Haiku under an Opus session).
    pub model: String,
    pub activity: Activity,
    pub last_activity: DateTime<Utc>,
    /// When the sub-agent was launched (its first transcript record).
    /// Immutable for the life of the agent, so it gives the rows a stable
    /// order: they sort by launch time and never reshuffle when one goes
    /// active/idle or appends a record — mirroring how the main session
    /// table holds pid order.
    pub started_at: DateTime<Utc>,
    /// Rolled-up total (input + output + cache) — the headline `tokens`
    /// column.
    pub tokens: u64,
    /// Full breakdown so the row can show in/out and cache columns like a
    /// session row. A sub-agent's spend lives entirely in its own
    /// transcript (zero sidechain records land in the parent), so these
    /// never double-count the parent session's totals.
    pub totals: crate::stats::UsageTotals,
    /// Name of the Workflow run this agent was spawned from, when it comes
    /// from a Workflow's internal fan-out rather than a plain Agent/Task
    /// delegation. `None` for the latter.
    pub workflow: Option<String>,
    /// Context currently in the sub-agent's window — the latest usage
    /// record's input + cache tokens, same math as the parent session's
    /// ctx column. `None` until the first usage record lands.
    pub current_context: Option<u64>,
    /// Context cap for the sub-agent's model: 1M for natively-1M
    /// families (Fable, Opus 4.8+) or once >200K has been observed,
    /// 200K otherwise.
    pub context_cap: Option<u64>,
}

/// Running sub-agents for the session whose transcript is `transcript_path`
/// and id is `session_id`, ordered most-recently-active first — both plain
/// Agent/Task delegations and agents a Workflow run has spawned
/// internally. Cheap when the session has neither: each source early-outs
/// on its own missing directory without reading the parent.
pub fn scan_running(transcript_path: &Path, session_id: &str) -> Vec<SubAgent> {
    let mut out = scan_flat_running(transcript_path, session_id);
    out.extend(scan_workflow_running(transcript_path, session_id));
    // Stable order: by launch time, oldest first, so a row never moves
    // when its agent's activity or token count changes. agent_id breaks
    // ties (e.g. agents launched in the same millisecond) deterministically.
    out.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    out
}

/// The plain Agent/Task half of [`scan_running`] — delegations launched by
/// a tool call in the parent transcript, resolved via its `tool_result`.
fn scan_flat_running(transcript_path: &Path, session_id: &str) -> Vec<SubAgent> {
    let Some(dir) = subagents_dir(transcript_path, session_id) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    // Pass 1 — cheap mtime gate. Collect only the agent files written
    // recently so a long delegation history isn't parsed every tick.
    let now = SystemTime::now();
    let mut fresh: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(agent_id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("agent-"))
            .map(String::from)
        else {
            continue;
        };
        let fresh_enough = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age <= FRESH_WINDOW);
        if fresh_enough {
            fresh.push((path, agent_id));
        }
    }
    if fresh.is_empty() {
        return Vec::new();
    }

    // Pass 2 — read the parent once for the set of finished delegations.
    let finished = finished_delegations(transcript_path);

    let mut out = Vec::new();
    for (path, agent_id) in fresh {
        let meta = read_meta(&path);
        // Skip any delegation the parent has already resolved — keyed on
        // the launching `toolUseId` (catches normal return and interrupt
        // alike), with the legacy agentId path as a fallback.
        let resolved = meta
            .as_ref()
            .and_then(|m| m.tool_use_id.as_deref())
            .is_some_and(|t| finished.tool_use_ids.contains(t))
            || finished.agent_ids.contains(&agent_id);
        if resolved {
            continue;
        }
        let Some(detail) = read_subagent(&path) else { continue };
        let agent_type = meta.as_ref().and_then(|m| m.agent_type.clone());
        let description = meta
            .as_ref()
            .and_then(|m| m.description.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_line(&detail.prompt));
        out.push(SubAgent {
            agent_id,
            transcript_path: path,
            agent_type,
            description,
            model: detail.model,
            activity: detail.activity,
            last_activity: detail.last_activity,
            started_at: detail.started_at,
            tokens: detail.totals.total_tokens(),
            totals: detail.totals,
            workflow: None,
            current_context: detail.current_context,
            context_cap: detail.context_cap,
        });
    }
    out
}

/// Agents currently running inside a Workflow's internal fan-out —
/// `agent()` calls a workflow script made itself, as opposed to a plain
/// Agent/Task delegation. See the module docs for the on-disk layout;
/// this mirrors [`scan_running`]'s two-pass shape (mtime gate, then a
/// cheap read for the terminal signal) with `journal.jsonl` standing in
/// for the parent transcript.
fn scan_workflow_running(transcript_path: &Path, session_id: &str) -> Vec<SubAgent> {
    let Some(root) = workflow_subagents_root(transcript_path, session_id) else {
        return Vec::new();
    };
    let Ok(run_dirs) = std::fs::read_dir(&root) else {
        return Vec::new();
    };

    let now = SystemTime::now();
    let mut out = Vec::new();
    for run_dir in run_dirs.flatten() {
        let dir = run_dir.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(run_id) = dir.file_name().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };

        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut fresh: Vec<(PathBuf, String)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(agent_id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("agent-"))
                .map(String::from)
            else {
                continue; // `journal.jsonl` itself
            };
            let fresh_enough = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age <= FRESH_WINDOW);
            if fresh_enough {
                fresh.push((path, agent_id));
            }
        }
        if fresh.is_empty() {
            continue;
        }

        let finished = finished_workflow_agents(&dir.join("journal.jsonl"));
        let run_meta = read_workflow_run(transcript_path, session_id, &run_id);

        for (path, agent_id) in fresh {
            if finished.contains(&agent_id) {
                continue;
            }
            let Some(detail) = read_subagent(&path) else { continue };
            let meta = read_meta(&path);
            let progress = run_meta.as_ref().and_then(|r| r.agents.get(&agent_id));
            let agent_type = progress
                .and_then(|p| p.agent_type.clone())
                .or_else(|| meta.as_ref().and_then(|m| m.agent_type.clone()));
            let description = progress
                .map(|p| p.label.clone())
                .or_else(|| meta.as_ref().and_then(|m| m.description.clone()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| first_line(&detail.prompt));
            out.push(SubAgent {
                agent_id,
                transcript_path: path,
                agent_type,
                description,
                model: detail.model,
                activity: detail.activity,
                last_activity: detail.last_activity,
                started_at: detail.started_at,
                tokens: detail.totals.total_tokens(),
                totals: detail.totals,
                workflow: Some(
                    run_meta
                        .as_ref()
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| run_id.clone()),
                ),
                current_context: detail.current_context,
                context_cap: detail.context_cap,
            });
        }
    }
    out
}

/// `<project>/<sessionId>/subagents/workflows` — one subdirectory per
/// active Workflow run, each laid out like the flat `subagents/` dir.
fn workflow_subagents_root(transcript_path: &Path, session_id: &str) -> Option<PathBuf> {
    Some(
        transcript_path
            .parent()?
            .join(session_id)
            .join("subagents")
            .join("workflows"),
    )
}

/// AgentIds a Workflow run's journal has already resolved — any record
/// besides `started` (`result`, and `error` should Claude Code ever emit
/// it) closes out that agent, mirroring `finished_delegations`' terminal
/// `tool_result` check.
fn finished_workflow_agents(journal_path: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(content) = std::fs::read_to_string(journal_path) else {
        return out;
    };
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let is_started = v.get("type").and_then(|x| x.as_str()) == Some("started");
        if is_started {
            continue;
        }
        if let Some(id) = v.get("agentId").and_then(|x| x.as_str()) {
            out.insert(id.to_string());
        }
    }
    out
}

/// The concise per-agent `label` (and agent type) a Workflow run's summary
/// carries, keyed by `agentId` — a much better row description than the
/// sidecar (which has no `description` field for workflow agents) or the
/// agent's own, often-huge prompt.
struct WorkflowRun {
    name: String,
    agents: std::collections::HashMap<String, WorkflowAgentProgress>,
}

struct WorkflowAgentProgress {
    label: String,
    agent_type: Option<String>,
}

/// Read `<project>/<sessionId>/workflows/<runId>.json`. Best-effort: a
/// resumed session can keep appending to an older session's
/// `subagents/workflows/<runId>/` directory while its own `workflows/`
/// folder holds a fresh run summary under the same id, so a miss here
/// just falls back to the sidecar/prompt for the label.
fn read_workflow_run(
    transcript_path: &Path,
    session_id: &str,
    run_id: &str,
) -> Option<WorkflowRun> {
    let path = transcript_path
        .parent()?
        .join(session_id)
        .join("workflows")
        .join(format!("{run_id}.json"));
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let name = v.get("workflowName").and_then(|x| x.as_str())?.to_string();
    let mut agents = std::collections::HashMap::new();
    if let Some(items) = v.get("workflowProgress").and_then(|x| x.as_array()) {
        for item in items {
            if item.get("type").and_then(|x| x.as_str()) != Some("workflow_agent") {
                continue;
            }
            let Some(agent_id) = item.get("agentId").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(label) = item.get("label").and_then(|x| x.as_str()) else {
                continue;
            };
            agents.insert(
                agent_id.to_string(),
                WorkflowAgentProgress {
                    label: label.to_string(),
                    agent_type: item
                        .get("agentType")
                        .and_then(|x| x.as_str())
                        .map(String::from),
                },
            );
        }
    }
    Some(WorkflowRun { name, agents })
}

/// `<project>/<sessionId>/subagents` — the directory Claude Code writes a
/// session's delegated-agent transcripts into, alongside (one level under)
/// the canonical `<sessionId>.jsonl`.
fn subagents_dir(transcript_path: &Path, session_id: &str) -> Option<PathBuf> {
    Some(transcript_path.parent()?.join(session_id).join("subagents"))
}

/// The two views of "this delegation has returned" we can read from the
/// parent transcript.
#[derive(Default)]
struct Finished {
    /// `toolUseId`s with a terminal `tool_result` (the primary signal —
    /// fires on completion and on interrupt/rejection). Excludes the
    /// `async_launched` ack a background agent receives at launch.
    tool_use_ids: HashSet<String>,
    /// agentIds with a terminal `toolUseResult` envelope. Legacy fallback
    /// for transcripts whose sidecar (and thus `toolUseId`) is missing.
    agent_ids: HashSet<String>,
}

/// Scan the parent transcript for every delegation it has already
/// resolved. Cheap relative to the full session — one sequential read,
/// only performed when at least one fresh sub-agent file exists.
fn finished_delegations(path: &Path) -> Finished {
    let mut out = Finished::default();
    let Ok(content) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // `async_launched` only announces that a background agent has
        // *started* — its tool_result is not terminal and must not retire
        // the row. Read the record-level status once and let it gate both
        // signals below.
        let tur = v.get("toolUseResult");
        let status = tur.and_then(|t| t.get("status")).and_then(|x| x.as_str());
        let is_async_launch = status == Some("async_launched");

        // Primary: a `tool_result` block resolves its `tool_use_id`.
        if let Some(items) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for ci in items {
                if ci.get("type").and_then(|x| x.as_str()) != Some("tool_result") {
                    continue;
                }
                if is_async_launch {
                    continue;
                }
                if let Some(tid) = ci.get("tool_use_id").and_then(|x| x.as_str()) {
                    out.tool_use_ids.insert(tid.to_string());
                }
            }
        }

        // Fallback: the `toolUseResult` envelope carries the agentId.
        if let Some(id) = tur.and_then(|t| t.get("agentId")).and_then(|x| x.as_str()) {
            if !is_async_launch {
                out.agent_ids.insert(id.to_string());
            }
        }
    }
    out
}

/// The `.meta.json` sidecar Claude Code writes beside each sub-agent
/// transcript: the friendly label plus the launching tool-call id.
struct Meta {
    tool_use_id: Option<String>,
    agent_type: Option<String>,
    description: Option<String>,
}

fn read_meta(agent_path: &Path) -> Option<Meta> {
    // `agent-<id>.jsonl` → `agent-<id>.meta.json` (agent ids never contain
    // a `.`, so replacing the extension is safe).
    let mut p = agent_path.to_path_buf();
    p.set_extension("meta.json");
    let raw = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    Some(Meta {
        tool_use_id: s("toolUseId"),
        agent_type: s("agentType"),
        description: s("description"),
    })
}

/// Per-sub-agent detail read from its own transcript.
struct SubDetail {
    /// First user prompt — a label fallback when the sidecar is missing.
    prompt: String,
    /// Timestamp of the sub-agent's first transcript record — its launch
    /// moment, used as the stable sort key.
    started_at: DateTime<Utc>,
    model: String,
    totals: crate::stats::UsageTotals,
    activity: Activity,
    last_activity: DateTime<Utc>,
    current_context: Option<u64>,
    context_cap: Option<u64>,
}

fn read_subagent(path: &Path) -> Option<SubDetail> {
    // Tokens + model come from the shared usage parse (cached on
    // (mtime, size), so re-reads of an unchanged file are free).
    let records = crate::stats::parse_file_cached(path).unwrap_or_default();
    let mut totals = crate::stats::UsageTotals::default();
    let mut model = String::new();
    let mut last_activity = DateTime::<Utc>::MIN_UTC;
    // Context tracking mirrors `stats::current_context_from_transcript`:
    // the newest record's input + cache tokens is what currently fills
    // the window; the max ever observed reveals the 1M tier. Records
    // are in file (append) order, so the last non-zero one wins.
    let mut current_context: Option<u64> = None;
    let mut max_context: u64 = 0;
    for r in &records {
        totals.add(r);
        if r.timestamp >= last_activity {
            last_activity = r.timestamp;
            model = r.model.clone();
        }
        let ctx = r.input + r.cache_read + r.cache_write_5m + r.cache_write_1h;
        if ctx > 0 {
            current_context = Some(ctx);
            max_context = max_context.max(ctx);
        }
    }
    let context_cap = current_context.map(|_| {
        if max_context > 200_000 || crate::stats::native_1m_context(&model) {
            1_000_000
        } else {
            200_000
        }
    });

    let (prompt, first_ts) = first_user_record(path).unwrap_or_default();
    // Launch time: the first record's own timestamp is the most stable
    // (it never changes once written); fall back to the file mtime for a
    // record that carried none.
    let mtime = || {
        std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(Utc::now)
    };
    let started_at = first_ts.unwrap_or_else(mtime);
    if last_activity == DateTime::<Utc>::MIN_UTC {
        // No usage records yet (the agent just started) — fall back so the
        // row still ages sensibly.
        last_activity = started_at;
    }

    let activity = match live_session::tail_activity(path) {
        Some(TailKind::PendingTool(a)) | Some(TailKind::Completed(a)) => a,
        None => Activity::Starting,
    };

    Some(SubDetail {
        prompt,
        started_at,
        model,
        totals,
        activity,
        last_activity,
        current_context,
        context_cap,
    })
}

/// Read just the first `user` record — the delegation prompt — without
/// slurping the whole transcript, returning its text and timestamp. The
/// prompt is the first line in practice; the small scan window covers a
/// stray leading record.
fn first_user_record(path: &Path) -> Option<(String, Option<DateTime<Utc>>)> {
    let f = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("user") {
            continue;
        }
        if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
            if let Some(t) = live_session::user_text(content) {
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc));
                return Some((t, ts));
            }
        }
    }
    None
}

/// First non-empty, trimmed line of `s` — a compact label when no sidecar
/// description is available.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn first_line_skips_blank_leading_lines() {
        assert_eq!(first_line("\n\n  hello world \nnext"), "hello world");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn subagents_dir_nests_under_session() {
        let t = Path::new("/p/proj/abc.jsonl");
        assert_eq!(
            subagents_dir(t, "abc").unwrap(),
            Path::new("/p/proj/abc/subagents")
        );
    }

    #[test]
    fn finished_delegations_keys_on_tool_use_id_and_excludes_async() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // A normal return, an interrupt (is_error), and a background-agent
        // launch ack (must NOT be treated as finished).
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"tDONE"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"tERR","is_error":true}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","toolUseResult":{{"agentId":"aBG","status":"async_launched"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"tBG"}}]}}}}"#
        )
        .unwrap();
        let fin = finished_delegations(f.path());
        assert!(fin.tool_use_ids.contains("tDONE"));
        assert!(fin.tool_use_ids.contains("tERR")); // interrupt still finishes the row
        assert!(!fin.tool_use_ids.contains("tBG")); // async launch isn't terminal
        assert!(!fin.agent_ids.contains("aBG"));
    }

    #[test]
    fn scan_running_empty_when_no_dir() {
        let t = Path::new("/nonexistent/proj/zzz.jsonl");
        assert!(scan_running(t, "zzz").is_empty());
    }

    #[test]
    fn scan_running_surfaces_live_agents_and_hides_resolved() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        // Parent: a finished delegation (toolUseId tB) and a still-running
        // background launch (toolUseId tC, async_launched → not finished).
        let parent = format!(
            "{}\n{}\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tB"}]}}"#,
            r#"{"type":"user","toolUseResult":{"agentId":"ccc","status":"async_launched"},"message":{"content":[{"type":"tool_result","tool_use_id":"tC"}]}}"#,
        );
        fs::write(&parent_path, parent).unwrap();

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();
        let write_agent = |id: &str, tool_use_id: &str, desc: &str, launched: &str| {
            let body = format!(
                "{}\n{}\n{}\n",
                format!(
                    r#"{{"type":"user","timestamp":"{launched}","message":{{"content":"do the work"}}}}"#
                ),
                r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
            );
            fs::write(subdir.join(format!("agent-{id}.jsonl")), body).unwrap();
            let meta = format!(
                r#"{{"agentType":"Explore","description":"{desc}","toolUseId":"{tool_use_id}"}}"#
            );
            fs::write(subdir.join(format!("agent-{id}.meta.json")), meta).unwrap();
        };
        // `ccc` is alphabetically last but launched first — the row order
        // must follow launch time, not agent_id, and not activity.
        write_agent("aaa", "tA", "Running A", "2026-06-19T00:00:05Z"); // live, later launch
        write_agent("bbb", "tB", "Done B", "2026-06-19T00:00:03Z"); // tB resolved → hidden
        write_agent("ccc", "tC", "Background C", "2026-06-19T00:00:02Z"); // live, earliest

        let subs = scan_running(&parent_path, sid);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ccc", "aaa"],
            "resolved agent hidden; survivors ordered by launch time"
        );
        let a = subs.iter().find(|s| s.agent_id == "aaa").unwrap();
        assert_eq!(a.agent_type.as_deref(), Some("Explore"));
        assert_eq!(a.description, "Running A");
        assert_eq!(a.model, "claude-haiku-4-5");
        assert_eq!(a.activity, Activity::Reading);
        assert_eq!(a.tokens, 15);
        assert_eq!(a.totals.input, 10);
        assert_eq!(a.totals.output, 5);
        assert_eq!(a.transcript_path, subdir.join("agent-aaa.jsonl"));
        // ctx = input + cache tokens of the newest usage record; haiku
        // with <200K observed sits on the 200K cap.
        assert_eq!(a.current_context, Some(10));
        assert_eq!(a.context_cap, Some(200_000));
    }

    #[test]
    fn finished_workflow_agents_reads_journal_terminal_records() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"started","agentId":"aLIVE"}}"#).unwrap();
        writeln!(f, r#"{{"type":"started","agentId":"aDONE"}}"#).unwrap();
        writeln!(f, r#"{{"type":"result","agentId":"aDONE"}}"#).unwrap();
        let fin = finished_workflow_agents(f.path());
        assert!(fin.contains("aDONE"));
        assert!(!fin.contains("aLIVE"));
    }

    #[test]
    fn scan_running_surfaces_live_workflow_agents_with_run_label() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        fs::write(&parent_path, "").unwrap();

        let run_id = "wf_abc123";
        let write_agent = |id: &str, launched: &str| {
            let dir = proj
                .path()
                .join(sid)
                .join("subagents")
                .join("workflows")
                .join(run_id);
            fs::create_dir_all(&dir).unwrap();
            let body = format!(
                "{}\n{}\n",
                format!(
                    r#"{{"type":"user","timestamp":"{launched}","message":{{"content":"do the work"}}}}"#
                ),
                r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":20,"output_tokens":8}}}"#,
            );
            fs::write(dir.join(format!("agent-{id}.jsonl")), body).unwrap();
            fs::write(
                dir.join(format!("agent-{id}.meta.json")),
                r#"{"agentType":"Explore","spawnDepth":1}"#,
            )
            .unwrap();
        };
        write_agent("aLIVE", "2026-06-19T00:00:05Z");
        write_agent("aDONE", "2026-06-19T00:00:03Z");
        fs::write(
            proj.path()
                .join(sid)
                .join("subagents")
                .join("workflows")
                .join(run_id)
                .join("journal.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                r#"{"type":"started","agentId":"aLIVE"}"#,
                r#"{"type":"started","agentId":"aDONE"}"#,
                r#"{"type":"result","agentId":"aDONE"}"#,
            ),
        )
        .unwrap();

        // Run summary supplies the concise label the sidecar can't.
        let run_dir = proj.path().join(sid).join("workflows");
        fs::create_dir_all(&run_dir).unwrap();
        let run_json = serde_json::json!({
            "workflowName": "review-things",
            "workflowProgress": [
                {"type": "workflow_agent", "agentId": "aLIVE", "label": "review:pipeline", "agentType": "Explore"},
            ],
        });
        fs::write(run_dir.join(format!("{run_id}.json")), run_json.to_string()).unwrap();

        let subs = scan_running(&parent_path, sid);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["aLIVE"], "resolved workflow agent hidden");
        let a = &subs[0];
        assert_eq!(a.workflow.as_deref(), Some("review-things"));
        assert_eq!(a.description, "review:pipeline");
        assert_eq!(a.agent_type.as_deref(), Some("Explore"));
        assert_eq!(a.tokens, 28);
    }
}
