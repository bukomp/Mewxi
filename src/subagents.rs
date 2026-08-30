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
//!     lingering. The one exception is the ack a background agent gets
//!     at launch ("Async agent launched successfully…") — that is not
//!     terminal and is excluded. The session transcript flags it with a
//!     `toolUseResult.status == "async_launched"` envelope; a sub-agent's
//!     own transcript carries no `toolUseResult` envelope at all, so the
//!     ack is also recognised by its text.
//!   * a **background** agent is finished when its parent receives the
//!     `<task-notification>` user record Claude Code injects once the
//!     agent stops (`<task-id>` = agentId). An agent continued afterwards
//!     via `SendMessage` writes to its transcript again, so a notification
//!     older than the transcript's mtime is ignored and the row comes
//!     back.
//!   * sub-agent transcripts accumulate for the whole life of the
//!     session, so a cheap mtime [`FRESH_WINDOW`] gate runs first: a
//!     session with hundreds of past delegations only ever parses the
//!     handful written recently. It also retires transcripts orphaned by
//!     a crash, and forked-skill agents (`/code-review` run as a fork),
//!     whose sidecar has no `toolUseId` to resolve.
//!
//! The parent transcript is read only when at least one fresh sub-agent
//! file exists.
//!
//! A sub-agent can itself spawn further sub-agents — an Agent-tool call
//! made from inside another sub-agent, or a `fork` agent. These nested
//! agents land flat in the very same `subagents/` directory (no nested
//! directories), with two extra `.meta.json` fields: `parentAgentId` (the
//! spawning agent's id) and `spawnDepth` (1 = spawned by the main agent, 2
//! = spawned by a depth-1 agent, …; absent on sidecars predating the
//! field, treated as depth 1). Resolution still follows the `toolUseId` →
//! `tool_result` rule, but is read from the *parent agent's own*
//! transcript (`agent-<parentAgentId>.jsonl`) rather than the session
//! transcript — each agent is judged solely by its own signal, so a
//! long-lived background child is never hidden just because its parent has
//! resolved. A parent blocked on its child's Agent-tool call can go quiet
//! in its own transcript for longer than [`FRESH_WINDOW`]; to avoid
//! orphaning the child's row, an otherwise-stale ancestor is pulled back in
//! and kept alive as long as any of its descendants is still fresh. The
//! combined result is ordered as a depth-first tree (see [`scan_running`])
//! instead of a flat sort, so a child renders immediately under its
//! parent.
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
//!
//! A third, higher-fidelity label source sits on top of both layouts: the
//! `subagentStatusLine` feed (see [`crate::agent_status`]) mewxi wires into
//! the account settings, which Claude Code refreshes every few seconds with
//! the exact caption its own agent panel shows for each running agent. When
//! present it is matched by agent id, falling back to the launch
//! `description`, and wins over both the sidecar description and the
//! transcript-derived `narration` — see [`SubAgent::status_label`].

use crate::live_session::{self, Activity, TailKind};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
    /// Agent id of the sub-agent that spawned this one — set when this
    /// agent is nested (an Agent-tool call made from inside another
    /// sub-agent, or a `fork` agent). `None` when the session's main agent
    /// spawned it directly.
    pub parent_agent_id: Option<String>,
    /// Nesting depth from the sidecar's `spawnDepth` (1 = spawned by the
    /// main agent, 2 = spawned by a depth-1 agent, …). Defaults to 1 when
    /// the sidecar is missing or predates the field.
    pub depth: u32,
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
    /// Detailed caption of the tool call the agent is on right now (e.g.
    /// `Read(view_all.rs)`), refreshed every scan from the transcript
    /// tail — the live counterpart to the static `description` above.
    /// `None` when the agent is thinking/responding rather than in a tool
    /// call.
    pub current_action: Option<String>,
    /// The agent's most recent narration line — the last assistant text
    /// block in its transcript ("Now checking the rendering code…").
    /// This is what the row shows *instead of* `description` once the
    /// agent starts talking: the launch description goes stale the moment
    /// work begins, the narration tracks it. `None` until the first text
    /// block lands.
    pub narration: Option<String>,
    /// The exact live summary Claude Code's own agent panel shows for this
    /// agent, read from the `subagentStatusLine` feed mewxi wires into the
    /// account settings ("Reading errorHandler.ts response mapping") — the
    /// highest-fidelity caption available. `None` whenever the feed isn't
    /// wired, hasn't fired yet, or has gone stale; consumers then fall back
    /// to `narration`/`description`.
    pub status_label: Option<String>,
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
/// and id is `session_id`, ordered as a depth-first tree — both plain
/// Agent/Task delegations and agents a Workflow run has spawned
/// internally, plus anything nested underneath either. Cheap when the
/// session has neither: each source early-outs on its own missing
/// directory without reading the parent.
pub fn scan_running(
    transcript_path: &Path,
    session_id: &str,
    account_dir: Option<&Path>,
) -> Vec<SubAgent> {
    let feed = account_dir.and_then(|d| crate::agent_status::read_feed(d, session_id));
    let mut out = scan_flat_running(transcript_path, session_id, feed.as_ref());
    out.extend(scan_workflow_running(transcript_path, session_id, feed.as_ref()));
    order_as_tree(out)
}

/// Reorder a flat batch of agents into a depth-first tree so the TUI can
/// splice rows verbatim and a nested sub-agent renders directly under its
/// parent. A root is any agent whose `parent_agent_id` is `None` or whose
/// parent isn't in this same batch (hidden, resolved, or spawned from a
/// Workflow) — such an orphan still keeps its own sidecar `depth`, it just
/// renders at the top level. Roots and each parent's children are sorted
/// by the same stable `(started_at, agent_id)` key the flat scan used to
/// sort the whole list by, so a row never moves on its own as long as its
/// launch time and tree position don't change.
fn order_as_tree(agents: Vec<SubAgent>) -> Vec<SubAgent> {
    let emitted_ids: HashSet<String> = agents.iter().map(|a| a.agent_id.clone()).collect();
    let mut children: HashMap<String, Vec<SubAgent>> = HashMap::new();
    let mut roots: Vec<SubAgent> = Vec::new();
    for a in agents {
        match &a.parent_agent_id {
            Some(pid) if emitted_ids.contains(pid) => {
                children.entry(pid.clone()).or_default().push(a);
            }
            _ => roots.push(a),
        }
    }

    let key = |a: &SubAgent| (a.started_at, a.agent_id.clone());
    roots.sort_by_key(key);
    for kids in children.values_mut() {
        kids.sort_by_key(key);
    }

    fn push_subtree(agent: SubAgent, children: &mut HashMap<String, Vec<SubAgent>>, out: &mut Vec<SubAgent>) {
        let id = agent.agent_id.clone();
        out.push(agent);
        if let Some(kids) = children.remove(&id) {
            for kid in kids {
                push_subtree(kid, children, out);
            }
        }
    }

    let mut out = Vec::with_capacity(emitted_ids.len());
    for root in roots {
        push_subtree(root, &mut children, &mut out);
    }
    out
}

/// One candidate row while [`scan_flat_running`] is still deciding what to
/// emit: a fresh agent file, or a stale ancestor pulled in to keep a live
/// descendant's row from looking orphaned (see the module docs).
struct Candidate {
    path: PathBuf,
    meta: Option<Meta>,
    /// Last write to this agent's transcript — compared against a
    /// `<task-notification>` timestamp to tell a finished background
    /// agent from one continued via `SendMessage` afterwards.
    modified: Option<DateTime<Utc>>,
    /// Whether *this* agent's own transcript passed the mtime gate —
    /// ancestors pulled in only for a fresh descendant have this `false`.
    self_fresh: bool,
}

/// Grace added to a `<task-notification>` timestamp before a later
/// transcript write counts as the agent having been resumed: the parent
/// records the notification a beat after the child's final write, and
/// the child may flush a trailing record after that.
const RESUME_SLACK_SECS: i64 = 5;

/// Whether a `<task-notification>` recorded at `notified_at` (`None` when
/// the record carried no timestamp) still stands for an agent whose
/// transcript was last written at `modified`: it does unless the agent
/// wrote again clearly after the notification — the `SendMessage`
/// continuation case.
fn notification_stands(
    notified_at: Option<DateTime<Utc>>,
    modified: Option<DateTime<Utc>>,
) -> bool {
    match (notified_at, modified) {
        (Some(ts), Some(m)) => m <= ts + chrono::Duration::seconds(RESUME_SLACK_SECS),
        _ => true,
    }
}

/// The plain Agent/Task half of [`scan_running`] — delegations launched by
/// a tool call in a transcript, resolved via its `tool_result`. For a
/// depth-1 agent that transcript is the session's; for a nested agent
/// (sidecar `parentAgentId` set) it is its parent agent's own transcript
/// instead, per the module docs.
fn scan_flat_running(
    transcript_path: &Path,
    session_id: &str,
    feed: Option<&crate::agent_status::AgentStatusFeed>,
) -> Vec<SubAgent> {
    let Some(dir) = subagents_dir(transcript_path, session_id) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    // Pass 1 — cheap mtime gate. Collect only the agent files written
    // recently so a long delegation history isn't parsed every tick.
    let now = SystemTime::now();
    let mut candidates: HashMap<String, Candidate> = HashMap::new();
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
        let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
        let fresh_enough = modified
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age <= FRESH_WINDOW);
        if fresh_enough {
            let meta = read_meta(&path);
            candidates.insert(
                agent_id,
                Candidate {
                    path,
                    meta,
                    modified: modified.map(DateTime::<Utc>::from),
                    self_fresh: true,
                },
            );
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    // Pull in ancestors of every fresh agent: a parent blocked on its
    // child's Agent-tool call can go quiet in its own transcript for
    // longer than FRESH_WINDOW, and without this it would vanish from the
    // list while its live child stayed — an orphan row. Recurses so a
    // depth-3 agent keeps both of its ancestors alive too. Each pulled-in
    // ancestor is judged the same as any other candidate below: not
    // resolved, and either fresh itself or kept alive by a fresh
    // descendant.
    let mut frontier: Vec<String> = candidates.keys().cloned().collect();
    while let Some(id) = frontier.pop() {
        let Some(parent_id) = candidates
            .get(&id)
            .and_then(|c| c.meta.as_ref())
            .and_then(|m| m.parent_agent_id.clone())
        else {
            continue;
        };
        if candidates.contains_key(&parent_id) {
            continue;
        }
        let parent_path = dir.join(format!("agent-{parent_id}.jsonl"));
        if !parent_path.is_file() {
            continue;
        }
        let modified = std::fs::metadata(&parent_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let self_fresh = modified
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age <= FRESH_WINDOW);
        let meta = read_meta(&parent_path);
        candidates.insert(
            parent_id.clone(),
            Candidate {
                path: parent_path,
                meta,
                modified: modified.map(DateTime::<Utc>::from),
                self_fresh,
            },
        );
        frontier.push(parent_id);
    }

    // Pass 2 — resolve each candidate against its own resolving transcript
    // (the session's for a depth-1 agent, its parent agent's own transcript
    // for a nested one), reading each distinct transcript at most once per
    // scan regardless of how many siblings share it.
    let mut finished_cache: HashMap<Option<String>, Finished> = HashMap::new();
    let mut resolved_ids: HashSet<String> = HashSet::new();
    for (agent_id, cand) in &candidates {
        let parent_key = cand.meta.as_ref().and_then(|m| m.parent_agent_id.clone());
        let finished = finished_cache.entry(parent_key.clone()).or_insert_with(|| {
            let resolving_path = match &parent_key {
                Some(pid) => dir.join(format!("agent-{pid}.jsonl")),
                None => transcript_path.to_path_buf(),
            };
            finished_delegations(&resolving_path)
        });
        // Skip any delegation its resolving transcript has already
        // recorded a `tool_result` for — keyed on the launching
        // `toolUseId` (catches normal return and interrupt alike), with
        // the legacy agentId path as a fallback — or, for a background
        // agent, a `<task-notification>` not superseded by a later write
        // to the agent's own transcript. Judged purely on this agent's
        // own signal: a resolved parent does not cascade-hide its
        // children, which can legitimately outlive it as background work.
        let resolved = cand
            .meta
            .as_ref()
            .and_then(|m| m.tool_use_id.as_deref())
            .is_some_and(|t| finished.tool_use_ids.contains(t))
            || finished.agent_ids.contains(agent_id)
            || finished
                .notified
                .get(agent_id)
                .is_some_and(|ts| notification_stands(*ts, cand.modified));
        if resolved {
            resolved_ids.insert(agent_id.clone());
        }
    }

    let mut out = Vec::new();
    for (agent_id, cand) in &candidates {
        if resolved_ids.contains(agent_id) {
            continue;
        }
        let keep_alive =
            cand.self_fresh || has_fresh_descendant(agent_id, &candidates, &resolved_ids);
        if !keep_alive {
            continue;
        }
        let Some(detail) = read_subagent(&cand.path) else { continue };
        let meta = &cand.meta;
        let agent_type = meta.as_ref().and_then(|m| m.agent_type.clone());
        let description = meta
            .as_ref()
            .and_then(|m| m.description.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_line(&detail.prompt));
        let status_label = feed.and_then(|f| {
            f.labels_by_id
                .get(agent_id)
                .or_else(|| f.labels_by_description.get(&description))
                .cloned()
        });
        out.push(SubAgent {
            agent_id: agent_id.clone(),
            parent_agent_id: meta.as_ref().and_then(|m| m.parent_agent_id.clone()),
            depth: meta.as_ref().and_then(|m| m.spawn_depth).unwrap_or(1),
            transcript_path: cand.path.clone(),
            agent_type,
            description,
            model: detail.model,
            activity: detail.activity,
            current_action: detail.current_action,
            narration: detail.narration,
            status_label,
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

/// Whether any transitive child of `agent_id` among `candidates` is itself
/// fresh — the signal that keeps a quiet-but-unresolved ancestor's row
/// alive (see [`scan_flat_running`]'s ancestor pull-in pass). A resolved
/// candidate can't carry the chain further since its own row won't be
/// emitted either way, but its still-live children are unaffected — they
/// are judged independently, not cascade-hidden.
fn has_fresh_descendant(
    agent_id: &str,
    candidates: &HashMap<String, Candidate>,
    resolved_ids: &HashSet<String>,
) -> bool {
    for (id, cand) in candidates {
        if resolved_ids.contains(id) {
            continue;
        }
        if cand.meta.as_ref().and_then(|m| m.parent_agent_id.as_deref()) != Some(agent_id) {
            continue;
        }
        if cand.self_fresh || has_fresh_descendant(id, candidates, resolved_ids) {
            return true;
        }
    }
    false
}

/// Agents currently running inside a Workflow's internal fan-out —
/// `agent()` calls a workflow script made itself, as opposed to a plain
/// Agent/Task delegation. See the module docs for the on-disk layout;
/// this mirrors [`scan_running`]'s two-pass shape (mtime gate, then a
/// cheap read for the terminal signal) with `journal.jsonl` standing in
/// for the parent transcript.
fn scan_workflow_running(
    transcript_path: &Path,
    session_id: &str,
    feed: Option<&crate::agent_status::AgentStatusFeed>,
) -> Vec<SubAgent> {
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
            let status_label = feed.and_then(|f| {
                f.labels_by_id
                    .get(&agent_id)
                    .or_else(|| f.labels_by_description.get(&description))
                    .cloned()
            });
            out.push(SubAgent {
                parent_agent_id: meta.as_ref().and_then(|m| m.parent_agent_id.clone()),
                depth: meta.as_ref().and_then(|m| m.spawn_depth).unwrap_or(1),
                agent_id,
                transcript_path: path,
                agent_type,
                description,
                model: detail.model,
                activity: detail.activity,
                current_action: detail.current_action,
                narration: detail.narration,
                status_label,
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
    /// Background agents (by agentId) the transcript has received a
    /// `<task-notification>` for, with the notification record's
    /// timestamp (`None` if it carried none). A later notification for
    /// the same agent replaces an earlier one, so a `SendMessage`
    /// continuation that stops again is judged by its latest stop.
    notified: HashMap<String, Option<DateTime<Utc>>>,
}

/// Text a background-agent launch ack opens with, in both the session
/// transcript and a sub-agent's own. Only the latter needs it: the
/// session transcript also flags the ack with a
/// `toolUseResult.status == "async_launched"` envelope, but sub-agent
/// transcripts carry no `toolUseResult` at all.
const ASYNC_LAUNCH_ACK_PREFIX: &str = "Async agent launched";

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

        let content = v.get("message").and_then(|m| m.get("content"));

        // Primary: a `tool_result` block resolves its `tool_use_id` —
        // unless it is a launch ack, flagged by the envelope or (in a
        // sub-agent transcript, which has no envelope) by its text.
        if let Some(items) = content.and_then(|c| c.as_array()) {
            for ci in items {
                if ci.get("type").and_then(|x| x.as_str()) != Some("tool_result") {
                    continue;
                }
                if is_async_launch || is_async_launch_ack(ci) {
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

        // Background agents: the `<task-notification>` user record
        // injected when the agent stops names it in `<task-id>`.
        if v.get("type").and_then(|x| x.as_str()) == Some("user") {
            if let Some(id) = content
                .and_then(live_session::user_text)
                .as_deref()
                .and_then(task_notification_agent_id)
            {
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc));
                out.notified.insert(id.to_string(), ts);
            }
        }
    }
    out
}

/// Whether a `tool_result` block is the "Async agent launched
/// successfully…" ack a background agent's launch returns.
fn is_async_launch_ack(tool_result: &serde_json::Value) -> bool {
    // Only the opening text matters, so peek at it in place rather than
    // concatenating the whole result (which can be a large file read).
    let Some(content) = tool_result.get("content") else {
        return false;
    };
    let head = content.as_str().or_else(|| {
        content
            .as_array()?
            .iter()
            .find(|ci| ci.get("type").and_then(|x| x.as_str()) == Some("text"))?
            .get("text")?
            .as_str()
    });
    head.is_some_and(|t| t.trim_start().starts_with(ASYNC_LAUNCH_ACK_PREFIX))
}

/// The `<task-id>` of a `<task-notification>` block in a user record's
/// text, if the text carries one.
fn task_notification_agent_id(text: &str) -> Option<&str> {
    let block = text.split("<task-notification>").nth(1)?;
    let block = block.split("</task-notification>").next()?;
    let id = block
        .split("<task-id>")
        .nth(1)?
        .split("</task-id>")
        .next()?
        .trim();
    (!id.is_empty()).then_some(id)
}

/// The `.meta.json` sidecar Claude Code writes beside each sub-agent
/// transcript: the friendly label, the launching tool-call id, and — for a
/// nested sub-agent — its spawning agent's id and nesting depth.
struct Meta {
    tool_use_id: Option<String>,
    agent_type: Option<String>,
    description: Option<String>,
    /// Agent id of the sub-agent that spawned this one. `None` for a
    /// depth-1 agent (spawned by the session's main agent).
    parent_agent_id: Option<String>,
    /// Nesting depth (1 = spawned by the main agent, 2 = spawned by a
    /// depth-1 agent, …). `None` when the sidecar predates the field
    /// (older Claude Code) — callers default this to 1.
    spawn_depth: Option<u32>,
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
        parent_agent_id: s("parentAgentId"),
        // Real sidecar carries this as a JSON number, not a string.
        spawn_depth: v
            .get("spawnDepth")
            .and_then(|x| x.as_u64())
            .map(|n| n as u32),
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
    /// See [`SubAgent::current_action`].
    current_action: Option<String>,
    /// See [`SubAgent::narration`].
    narration: Option<String>,
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

    let status = live_session::tail_status(path);
    let activity = match status.as_ref().map(|s| &s.kind) {
        Some(TailKind::PendingTool(a)) | Some(TailKind::Completed(a)) => a.clone(),
        None => Activity::Starting,
    };
    let (current_action, narration) = match status {
        Some(s) => (s.action, s.narration),
        None => (None, None),
    };

    Some(SubDetail {
        prompt,
        started_at,
        model,
        totals,
        activity,
        current_action,
        narration,
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
        // The same ack as it lands in a *sub-agent's* transcript: no
        // `toolUseResult` envelope at all, only the text.
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"tBG2","content":[{{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata)\nagentId: aBG2 (internal ID)"}}]}}]}}}}"#
        )
        .unwrap();
        // The background agent's stop, injected as a task notification.
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-06-19T00:01:00Z","message":{{"role":"user","content":"[SYSTEM NOTIFICATION - NOT USER INPUT]\n<task-notification>\n<task-id>aBG2</task-id>\n<tool-use-id>tBG2</tool-use-id>\n<status>completed</status>\n</task-notification>"}}}}"#
        )
        .unwrap();
        let fin = finished_delegations(f.path());
        assert!(fin.tool_use_ids.contains("tDONE"));
        assert!(fin.tool_use_ids.contains("tERR")); // interrupt still finishes the row
        assert!(!fin.tool_use_ids.contains("tBG")); // async launch isn't terminal
        assert!(!fin.agent_ids.contains("aBG"));
        assert!(!fin.tool_use_ids.contains("tBG2")); // envelope-less ack isn't either
        assert_eq!(
            fin.notified.get("aBG2").copied().flatten(),
            Some("2026-06-19T00:01:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn task_notification_agent_id_extracts_task_id() {
        assert_eq!(
            task_notification_agent_id(
                "preamble\n<task-notification>\n<task-id> abc123 </task-id>\n<status>completed</status>\n</task-notification>"
            ),
            Some("abc123")
        );
        assert_eq!(task_notification_agent_id("<task-id>x</task-id>"), None);
        assert_eq!(
            task_notification_agent_id(
                "<task-notification><task-id></task-id></task-notification>"
            ),
            None
        );
    }

    #[test]
    fn notification_stands_unless_agent_wrote_after_it() {
        let ts = "2026-06-19T00:01:00Z".parse::<DateTime<Utc>>().unwrap();
        let secs = chrono::Duration::seconds;
        assert!(notification_stands(Some(ts), Some(ts - secs(10))));
        assert!(notification_stands(
            Some(ts),
            Some(ts + secs(RESUME_SLACK_SECS))
        ));
        assert!(!notification_stands(
            Some(ts),
            Some(ts + secs(RESUME_SLACK_SECS + 1))
        ));
        // Missing either timestamp: trust the notification.
        assert!(notification_stands(None, Some(ts)));
        assert!(notification_stands(Some(ts), None));
    }

    /// The reported case: `/code-review` run as a forked skill (sidecar
    /// without `toolUseId`) fans out background finders. Their launch
    /// acks land in the fork's transcript without a `toolUseResult`
    /// envelope and must not retire them; a `<task-notification>` must.
    #[test]
    fn scan_running_keeps_background_children_of_forked_skill_until_notified() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        // The session transcript only logs the fork's launch as a system record.
        fs::write(
            &parent_path,
            r#"{"type":"system","subtype":"local_command","content":"<forked-skill-launch>{\"agentId\":\"fork\",\"skillName\":\"code-review\"}</forked-skill-launch>"}"#,
        )
        .unwrap();

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();

        let ack = |tid: &str, aid: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{tid}","content":[{{"type":"text","text":"Async agent launched successfully. (internal metadata)\nagentId: {aid} (internal ID)"}}]}}]}}}}"#
            )
        };
        let notify = |aid: &str, ts: &str| {
            format!(
                r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"[SYSTEM NOTIFICATION]\n<task-notification>\n<task-id>{aid}</task-id>\n<status>completed</status>\n</task-notification>"}}}}"#
            )
        };
        // cDone's notification is recent (its transcript, stamped just
        // before it, stays inside FRESH_WINDOW — the notification, not
        // the mtime gate, must hide it). cBack's is old: the fixture
        // writes its transcript "now", long after, i.e. it was continued
        // via SendMessage.
        let notified_at = Utc::now() - chrono::Duration::seconds(2);
        let fork_lines = format!(
            "\n{}\n{}\n{}\n{}\n{}",
            ack("tLIVE", "cLive"),
            ack("tDONE", "cDone"),
            ack("tBACK", "cBack"),
            notify(
                "cDone",
                &notified_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            ),
            notify("cBack", "2026-06-19T00:01:00Z"),
        );
        write_nested_fixture(
            &subdir,
            "fork",
            r#"{"agentType":"general-purpose","description":"/code-review xhigh","name":"code-review","spawnDepth":1}"#,
            "2026-06-19T00:00:01Z",
            &fork_lines,
        );
        let child_meta = |tid: &str| {
            format!(
                r#"{{"agentType":"general-purpose","description":"finder","toolUseId":"{tid}","parentAgentId":"fork","spawnDepth":2}}"#
            )
        };
        write_nested_fixture(
            &subdir,
            "cLive",
            &child_meta("tLIVE"),
            "2026-06-19T00:00:02Z",
            "",
        );
        write_nested_fixture(
            &subdir,
            "cDone",
            &child_meta("tDONE"),
            "2026-06-19T00:00:03Z",
            "",
        );
        write_nested_fixture(
            &subdir,
            "cBack",
            &child_meta("tBACK"),
            "2026-06-19T00:00:04Z",
            "",
        );

        // cDone last wrote just before its notification.
        let written_at = notified_at - chrono::Duration::seconds(1);
        std::fs::OpenOptions::new()
            .write(true)
            .open(subdir.join("agent-cDone.jsonl"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(written_at.into()))
            .unwrap();

        let subs = scan_running(&parent_path, sid, None);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["fork", "cLive", "cBack"],
            "envelope-less launch acks keep children alive; a notification retires cDone; \
             cBack's later write overrides its notification"
        );
        let fork = &subs[0];
        assert_eq!(fork.parent_agent_id, None);
        assert_eq!(fork.description, "/code-review xhigh");
        assert!(subs[1..]
            .iter()
            .all(|s| s.parent_agent_id.as_deref() == Some("fork")));
    }

    #[test]
    fn scan_running_empty_when_no_dir() {
        let t = Path::new("/nonexistent/proj/zzz.jsonl");
        assert!(scan_running(t, "zzz", None).is_empty());
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
                "{}\n{}\n{}\n{}\n",
                format!(
                    r#"{{"type":"user","timestamp":"{launched}","message":{{"content":"do the work"}}}}"#
                ),
                r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Scanning the fixtures now"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/x/y/lib.rs"}}]}}"#,
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

        let subs = scan_running(&parent_path, sid, None);
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
        assert_eq!(a.current_action, Some("Read(lib.rs)".to_string()));
        // The live caption: the agent's latest narration, found past the
        // pending tool_use record.
        assert_eq!(a.narration.as_deref(), Some("Scanning the fixtures now"));
        assert_eq!(a.tokens, 15);
        assert_eq!(a.totals.input, 10);
        assert_eq!(a.totals.output, 5);
        assert_eq!(a.transcript_path, subdir.join("agent-aaa.jsonl"));
        // ctx = input + cache tokens of the newest usage record; haiku
        // with <200K observed sits on the 200K cap.
        assert_eq!(a.current_context, Some(10));
        assert_eq!(a.context_cap, Some(200_000));
        // Depth-1, un-nested delegation: no parent, sidecar-default depth.
        assert_eq!(a.parent_agent_id, None);
        assert_eq!(a.depth, 1);
    }

    #[test]
    fn scan_running_matches_status_label_from_agent_status_feed() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        fs::write(&parent_path, "").unwrap();

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();
        let write_agent = |id: &str, desc: &str, launched: &str| {
            let body = format!(
                "{}\n{}\n",
                format!(
                    r#"{{"type":"user","timestamp":"{launched}","message":{{"content":"do the work"}}}}"#
                ),
                r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            );
            fs::write(subdir.join(format!("agent-{id}.jsonl")), body).unwrap();
            let meta =
                format!(r#"{{"agentType":"Explore","description":"{desc}","toolUseId":"t{id}"}}"#);
            fs::write(subdir.join(format!("agent-{id}.meta.json")), meta).unwrap();
        };
        write_agent("aaa", "Running A", "2026-06-19T00:00:05Z");
        write_agent("bbb", "Running B", "2026-06-19T00:00:06Z");

        // Account dir is a separate tree from the project dir in real life.
        let account = tempfile::tempdir().unwrap();
        let sessions_dir = account.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::write(
            sessions_dir.join(format!("{sid}.agent-status.json")),
            r#"{"session_id":"sess1","tasks":[{"id":"aaa","description":"Running A","label":"Reading lib.rs for conversion"}]}"#,
        )
        .unwrap();

        let subs = scan_running(&parent_path, sid, Some(account.path()));
        let a = subs.iter().find(|s| s.agent_id == "aaa").unwrap();
        assert_eq!(
            a.status_label.as_deref(),
            Some("Reading lib.rs for conversion")
        );
        let b = subs.iter().find(|s| s.agent_id == "bbb").unwrap();
        assert_eq!(b.status_label, None);
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

        let subs = scan_running(&parent_path, sid, None);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["aLIVE"], "resolved workflow agent hidden");
        let a = &subs[0];
        assert_eq!(a.workflow.as_deref(), Some("review-things"));
        assert_eq!(a.description, "review:pipeline");
        assert_eq!(a.agent_type.as_deref(), Some("Explore"));
        assert_eq!(a.tokens, 28);
        // Tail is a usage record with no tool_use content — nothing to
        // caption, so the live action and narration stay unset.
        assert_eq!(a.current_action, None);
        assert_eq!(a.narration, None);
        // Workflow sidecars carry spawnDepth too; this one has no
        // parentAgentId, so it's a depth-1 root like a plain delegation.
        assert_eq!(a.parent_agent_id, None);
        assert_eq!(a.depth, 1);
    }

    #[test]
    fn read_meta_parses_parent_and_spawn_depth() {
        let dir = tempfile::tempdir().unwrap();

        let nested_path = dir.path().join("agent-cAgent.jsonl");
        fs::write(&nested_path, "").unwrap();
        fs::write(
            dir.path().join("agent-cAgent.meta.json"),
            r#"{"agentType":"Explore","description":"Trivial nested spawn probe","toolUseId":"tX","parentAgentId":"pAgent","spawnDepth":2}"#,
        )
        .unwrap();
        let nested = read_meta(&nested_path).unwrap();
        assert_eq!(nested.parent_agent_id.as_deref(), Some("pAgent"));
        assert_eq!(nested.spawn_depth, Some(2));

        // Depth-1 / legacy sidecar: neither field present.
        let root_path = dir.path().join("agent-root.jsonl");
        fs::write(&root_path, "").unwrap();
        fs::write(
            dir.path().join("agent-root.meta.json"),
            r#"{"agentType":"general-purpose","description":"root task"}"#,
        )
        .unwrap();
        let root = read_meta(&root_path).unwrap();
        assert_eq!(root.parent_agent_id, None);
        assert_eq!(root.spawn_depth, None);
    }

    /// Shared fixture builder for the nested-agent tests below: writes a
    /// minimal-but-valid sub-agent transcript (a launch record plus a
    /// usage record, so `read_subagent` succeeds) and its `.meta.json`
    /// sidecar, with room for extra hand-written lines (e.g. a
    /// `tool_result` resolving a child).
    fn write_nested_fixture(subdir: &Path, id: &str, meta_json: &str, launched: &str, extra_lines: &str) {
        let body = format!(
            "{}\n{}\n{}",
            format!(
                r#"{{"type":"user","timestamp":"{launched}","message":{{"content":"do the work"}}}}"#
            ),
            r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            extra_lines,
        );
        fs::write(subdir.join(format!("agent-{id}.jsonl")), body).unwrap();
        fs::write(subdir.join(format!("agent-{id}.meta.json")), meta_json).unwrap();
    }

    #[test]
    fn scan_running_surfaces_nested_subagent_when_parent_transcript_unresolved() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        fs::write(&parent_path, "").unwrap(); // session transcript resolves nothing

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();

        write_nested_fixture(
            &subdir,
            "pAgent",
            r#"{"agentType":"general-purpose","description":"Parent task","toolUseId":"tP","spawnDepth":1}"#,
            "2026-06-19T00:00:02Z",
            "",
        );
        write_nested_fixture(
            &subdir,
            "cAgent",
            r#"{"agentType":"Explore","description":"Child task","toolUseId":"tC","parentAgentId":"pAgent","spawnDepth":2}"#,
            "2026-06-19T00:00:04Z",
            "",
        );

        let subs = scan_running(&parent_path, sid, None);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["pAgent", "cAgent"],
            "both live; child renders right under its parent"
        );
        let p = subs.iter().find(|s| s.agent_id == "pAgent").unwrap();
        assert_eq!(p.parent_agent_id, None);
        assert_eq!(p.depth, 1);
        let c = subs.iter().find(|s| s.agent_id == "cAgent").unwrap();
        assert_eq!(c.parent_agent_id.as_deref(), Some("pAgent"));
        assert_eq!(c.depth, 2);
    }

    #[test]
    fn scan_running_retires_nested_subagent_once_parent_agent_transcript_resolves_it() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        fs::write(&parent_path, "").unwrap();

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();

        // P's own transcript (not the session's) records the tool_result
        // that resolves C's launching toolUseId — the signal a nested
        // delegation's liveness must be judged against.
        let p_resolves_child = format!(
            "\n{}",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tC"}]}}"#
        );
        write_nested_fixture(
            &subdir,
            "pAgent",
            r#"{"agentType":"general-purpose","description":"Parent task","toolUseId":"tP","spawnDepth":1}"#,
            "2026-06-19T00:00:02Z",
            &p_resolves_child,
        );
        write_nested_fixture(
            &subdir,
            "cAgent",
            r#"{"agentType":"Explore","description":"Child task","toolUseId":"tC","parentAgentId":"pAgent","spawnDepth":2}"#,
            "2026-06-19T00:00:04Z",
            "",
        );

        let subs = scan_running(&parent_path, sid, None);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["pAgent"],
            "child resolved via its parent's own transcript, not the session's"
        );
    }

    #[test]
    fn scan_running_keeps_stale_ancestor_alive_for_fresh_descendant() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        fs::write(&parent_path, "").unwrap();

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();

        write_nested_fixture(
            &subdir,
            "pAgent",
            r#"{"agentType":"general-purpose","description":"Parent task","toolUseId":"tP","spawnDepth":1}"#,
            "2026-06-19T00:00:02Z",
            "",
        );
        write_nested_fixture(
            &subdir,
            "cAgent",
            r#"{"agentType":"Explore","description":"Child task","toolUseId":"tC","parentAgentId":"pAgent","spawnDepth":2}"#,
            "2026-06-19T00:00:04Z",
            "",
        );

        // Push pAgent's transcript mtime outside FRESH_WINDOW (90s) while
        // leaving cAgent's untouched (freshly written = fresh).
        let p_path = subdir.join("agent-pAgent.jsonl");
        let stale = SystemTime::now() - Duration::from_secs(200);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&p_path)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();

        let subs = scan_running(&parent_path, sid, None);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["pAgent", "cAgent"],
            "stale-but-unresolved ancestor kept alive by its fresh child"
        );
    }

    #[test]
    fn scan_running_orders_as_dfs_tree_with_children_glued_under_parent() {
        let proj = tempfile::tempdir().unwrap();
        let sid = "sess1";
        let parent_path = proj.path().join(format!("{sid}.jsonl"));
        fs::write(&parent_path, "").unwrap();

        let subdir = proj.path().join(sid).join("subagents");
        fs::create_dir_all(&subdir).unwrap();

        write_nested_fixture(
            &subdir,
            "A",
            r#"{"agentType":"general-purpose","toolUseId":"tA"}"#,
            "2026-06-19T00:00:02Z",
            "",
        );
        write_nested_fixture(
            &subdir,
            "C1",
            r#"{"agentType":"Explore","toolUseId":"tC1","parentAgentId":"A","spawnDepth":2}"#,
            "2026-06-19T00:00:07Z",
            "",
        );
        write_nested_fixture(
            &subdir,
            "B",
            r#"{"agentType":"general-purpose","toolUseId":"tB"}"#,
            "2026-06-19T00:00:04Z",
            "",
        );

        let subs = scan_running(&parent_path, sid, None);
        let ids: Vec<_> = subs.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["A", "C1", "B"],
            "C1 sits directly under root A even though root B started earlier than C1"
        );
    }
}
