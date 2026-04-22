//! Minimal MCP server over stdio (JSON-RPC 2.0, protocol `2024-11-05`).
//!
//! Read-only; exposes tools that map 1:1 onto the `stats::Aggregate`
//! shape plus one passthrough to the live `/usage` endpoint. Tool
//! results are wrapped in the standard MCP `content: [{type: "text",
//! text: ...}]` envelope with the payload pretty-printed as JSON so
//! Claude can cite specific numbers verbatim.
//!
//! Each tool call currently triggers a fresh `load_and_aggregate` on a
//! blocking thread — the per-file cache keeps this cheap but a busy
//! client could still see tens of millis per call. If that ever
//! matters, cache the aggregate in-process with a TTL.

use crate::live_usage;
use crate::stats;
use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run(no_live: bool) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(resp) = handle(req, no_live).await {
            let mut s = serde_json::to_string(&resp)?;
            s.push('\n');
            stdout.write_all(s.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

async fn handle(req: Value, no_live: bool) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) do not get responses.
    let is_notification = id.is_none();

    match method {
        "initialize" => Some(ok(id, json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "claude-usage", "version": env!("CARGO_PKG_VERSION") }
        }))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tool_defs() }))),
        "tools/call" => Some(handle_tool_call(id, params, no_live).await),
        _ => {
            if is_notification {
                None
            } else {
                Some(err(id, -32601, format!("method not found: {method}")))
            }
        }
    }
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "get_totals",
            "description": "Return aggregate Claude Code usage totals (all time, today, this week, this month) with token breakdowns and estimated USD cost.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_today",
            "description": "Return today's Claude Code usage totals only.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_by_model",
            "description": "Return usage totals grouped by model, sorted by cost descending.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_by_project",
            "description": "Return usage totals grouped by project, sorted by cost descending. Optional `limit` caps the number of projects returned.",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 500 } },
                "additionalProperties": false
            }
        },
        {
            "name": "get_by_day",
            "description": "Return daily usage totals for the last N days (default 14).",
            "inputSchema": {
                "type": "object",
                "properties": { "days": { "type": "integer", "minimum": 1, "maximum": 365 } },
                "additionalProperties": false
            }
        },
        {
            "name": "get_recent",
            "description": "Return the most recent assistant messages with timestamp, model, tokens and cost. Optional `limit` (default 20).",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 200 } },
                "additionalProperties": false
            }
        },
        {
            "name": "get_live_usage",
            "description": "Return Claude Code's authoritative 5-hour and weekly usage percentages, as reported by api.anthropic.com/api/oauth/usage (the same source as the CLI's /usage command and status bar). May be absent if not authenticated via subscription, if --no-live is set, or if the endpoint is rate-limited and no cached value is available.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

async fn handle_tool_call(id: Option<Value>, params: Value, no_live: bool) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    // Live usage tool does its own fetch and returns early — no need to parse
    // the local JSONL corpus for this request.
    if name == "get_live_usage" {
        let live = tokio::task::spawn_blocking(move || live_usage::fetch_or_cached(no_live)).await;
        let result_json = match live {
            Ok(Some(l)) => serde_json::to_value(&l).unwrap_or(Value::Null),
            Ok(None) => json!({ "unavailable": true, "hint": "no credential, network blocked, or rate limited" }),
            Err(e) => return err(id, -32000, format!("join failed: {e}")),
        };
        let pretty = serde_json::to_string_pretty(&result_json).unwrap_or_else(|_| "{}".to_string());
        return ok(
            id,
            json!({
                "content": [{ "type": "text", "text": pretty }],
                "isError": false
            }),
        );
    }

    let agg = match tokio::task::spawn_blocking(stats::load_and_aggregate).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return err(id, -32000, format!("load failed: {e}")),
        Err(e) => return err(id, -32000, format!("join failed: {e}")),
    };

    let result_json = match name {
        "get_totals" => json!({
            "all_time": agg.all,
            "this_month": agg.this_month,
            "this_week": agg.this_week,
            "today": agg.today,
            "sessions": agg.sessions_count,
            "projects": agg.projects_count,
        }),
        "get_today" => serde_json::to_value(&agg.today).unwrap_or(Value::Null),
        "get_by_model" => {
            let mut v: Vec<_> = agg.by_model.iter().collect();
            v.sort_by(|a, b| b.1.cost_usd.partial_cmp(&a.1.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
            json!(v.iter().map(|(m, t)| json!({ "model": m, "totals": t })).collect::<Vec<_>>())
        }
        "get_by_project" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let mut v: Vec<_> = agg.by_project.iter().collect();
            v.sort_by(|a, b| b.1.cost_usd.partial_cmp(&a.1.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
            json!(v
                .into_iter()
                .take(limit)
                .map(|(p, t)| json!({ "project": p, "totals": t }))
                .collect::<Vec<_>>())
        }
        "get_by_day" => {
            let days = args.get("days").and_then(|v| v.as_u64()).unwrap_or(14) as usize;
            let mut v: Vec<_> = agg.by_day.iter().collect();
            v.sort_by(|a, b| b.0.cmp(a.0));
            json!(v
                .into_iter()
                .take(days)
                .map(|(d, t)| json!({ "date": d.to_string(), "totals": t }))
                .collect::<Vec<_>>())
        }
        "get_recent" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            json!(agg.recent.iter().take(limit).collect::<Vec<_>>())
        }
        other => return err(id, -32602, format!("unknown tool: {other}")),
    };

    // MCP wraps tool results in { content: [{ type: "text", text: "..." }], isError: false }
    let pretty = serde_json::to_string_pretty(&result_json).unwrap_or_else(|_| "{}".to_string());
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": pretty }],
            "isError": false
        }),
    )
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn err(id: Option<Value>, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": { "code": code, "message": message } })
}
