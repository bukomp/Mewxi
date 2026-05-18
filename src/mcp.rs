//! Minimal MCP server over stdio (JSON-RPC 2.0, protocol `2024-11-05`).
//!
//! Read-only. Every tool that reads usage data accepts an optional
//! `account` argument; when omitted, totals/breakdowns are summed
//! across every account discovered by [`accounts::load_accounts`]
//! and `by_project` keys are namespaced as `<account>/<project>` so
//! they remain unique. Two extra discovery tools — `list_accounts`
//! and `list_live_sessions` — expose the multi-account topology so
//! clients can pick an `account` to filter by.

use crate::accounts::{self, Account, AccountsView};
use crate::live_session;
use crate::live_usage;
use crate::stats::{self, Aggregate, UsageTotals};
use anyhow::Result;
use chrono::NaiveDate;
use serde_json::{json, Value};
use std::collections::BTreeMap;
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

fn account_schema() -> Value {
    json!({ "type": "string", "description": "Account name (see list_accounts). Omit to aggregate across all accounts." })
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "list_accounts",
            "description": "List every Claude Code account claude-usage knows about (one per CLAUDE_CONFIG_DIR), with its directory and configured default.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "list_live_sessions",
            "description": "List currently-active Claude Code conversations (JSONL transcripts whose mtime is within the live-session threshold) across every account. Filter by `account` to scope to one.",
            "inputSchema": {
                "type": "object",
                "properties": { "account": account_schema() },
                "additionalProperties": false
            }
        },
        {
            "name": "get_totals",
            "description": "Aggregate Claude Code usage totals (all time, today, this week, this month) with token breakdowns and estimated USD cost. Omit `account` to sum across every account.",
            "inputSchema": {
                "type": "object",
                "properties": { "account": account_schema() },
                "additionalProperties": false
            }
        },
        {
            "name": "get_today",
            "description": "Today's Claude Code usage totals. Omit `account` to sum across every account.",
            "inputSchema": {
                "type": "object",
                "properties": { "account": account_schema() },
                "additionalProperties": false
            }
        },
        {
            "name": "get_by_model",
            "description": "Usage totals grouped by model, sorted by cost descending. Omit `account` to sum across every account.",
            "inputSchema": {
                "type": "object",
                "properties": { "account": account_schema() },
                "additionalProperties": false
            }
        },
        {
            "name": "get_by_project",
            "description": "Usage totals grouped by project, sorted by cost descending. Without `account`, project keys are namespaced as `<account>/<project>` so they remain unique. `limit` caps the number of projects returned (default 50).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account_schema(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_by_day",
            "description": "Daily usage totals for the last N days (default 14). Omit `account` to sum across every account.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account_schema(),
                    "days": { "type": "integer", "minimum": 1, "maximum": 365 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_recent",
            "description": "Most recent assistant messages with timestamp, account, project, model, tokens, and cost. Omit `account` to interleave across every account by timestamp.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account_schema(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_live_usage",
            "description": "Claude Code's authoritative 5-hour and weekly usage percentages for a specific account, as reported by api.anthropic.com/api/oauth/usage. `account` defaults to the configured default account.",
            "inputSchema": {
                "type": "object",
                "properties": { "account": account_schema() },
                "additionalProperties": false
            }
        }
    ])
}

async fn handle_tool_call(id: Option<Value>, params: Value, no_live: bool) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let account_arg = args.get("account").and_then(|v| v.as_str()).map(String::from);

    let view = match accounts::load_accounts() {
        Ok(v) => v,
        Err(e) => return err(id, -32000, format!("accounts: {e}")),
    };

    // Discovery tools — no JSONL parsing needed.
    match name.as_str() {
        "list_accounts" => {
            let payload = json!({
                "default_account": view.default_account,
                "accounts": view.accounts.iter().map(|a| json!({
                    "name": a.name,
                    "dir": a.dir,
                })).collect::<Vec<_>>(),
            });
            return wrap_ok(id, payload);
        }
        "list_live_sessions" => {
            let accounts_to_scan: Vec<Account> = match account_arg.as_deref() {
                Some(n) => match view.accounts.iter().find(|a| a.name == n) {
                    Some(a) => vec![a.clone()],
                    None => return err(id, -32602, format!("unknown account: {n}")),
                },
                None => view.accounts.clone(),
            };
            let sessions: Vec<_> = tokio::task::spawn_blocking(move || {
                let alive = live_session::alive_pids();
                let mut out = Vec::new();
                for a in &accounts_to_scan {
                    out.extend(live_session::scan(a, &alive, &[]));
                }
                out.sort_by(|x, y| y.last_activity.cmp(&x.last_activity));
                out
            })
            .await
            .unwrap_or_default();
            return wrap_ok(id, json!(sessions));
        }
        _ => {}
    }

    // Live usage tool: per-account; defaults to default_account.
    if name == "get_live_usage" {
        let account = match pick_one(&view, account_arg.as_deref()) {
            Ok(a) => a,
            Err(e) => return err(id, -32602, e),
        };
        let live = tokio::task::spawn_blocking(move || live_usage::fetch_or_cached(&account, no_live)).await;
        let payload = match live {
            Ok(Some(l)) => serde_json::to_value(&l).unwrap_or(Value::Null),
            Ok(None) => json!({ "unavailable": true, "hint": "no credential, network blocked, or rate limited" }),
            Err(e) => return err(id, -32000, format!("join failed: {e}")),
        };
        return wrap_ok(id, payload);
    }

    // Stats tools: either one account or the union of all accounts.
    let scope: Vec<Account> = match account_arg.as_deref() {
        Some(n) => match view.accounts.iter().find(|a| a.name == n) {
            Some(a) => vec![a.clone()],
            None => return err(id, -32602, format!("unknown account: {n}")),
        },
        None => view.accounts.clone(),
    };

    let aggregated: Aggregated = match tokio::task::spawn_blocking(move || aggregate_scope(&scope)).await {
        Ok(a) => a,
        Err(e) => return err(id, -32000, format!("join failed: {e}")),
    };

    let payload = match name.as_str() {
        "get_totals" => json!({
            "all_time": aggregated.all,
            "this_month": aggregated.this_month,
            "this_week": aggregated.this_week,
            "today": aggregated.today,
            "sessions": aggregated.sessions_count,
            "projects": aggregated.projects_count,
            "accounts_in_scope": aggregated.accounts_in_scope,
        }),
        "get_today" => serde_json::to_value(&aggregated.today).unwrap_or(Value::Null),
        "get_by_model" => {
            let mut v: Vec<_> = aggregated.by_model.iter().collect();
            v.sort_by(|a, b| b.1.cost_usd.partial_cmp(&a.1.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
            json!(v.iter().map(|(m, t)| json!({ "model": m, "totals": t })).collect::<Vec<_>>())
        }
        "get_by_project" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let mut v: Vec<_> = aggregated.by_project.iter().collect();
            v.sort_by(|a, b| b.1.cost_usd.partial_cmp(&a.1.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
            json!(v
                .into_iter()
                .take(limit)
                .map(|(p, t)| json!({ "project": p, "totals": t }))
                .collect::<Vec<_>>())
        }
        "get_by_day" => {
            let days = args.get("days").and_then(|v| v.as_u64()).unwrap_or(14) as usize;
            let mut v: Vec<_> = aggregated.by_day.iter().collect();
            v.sort_by(|a, b| b.0.cmp(a.0));
            json!(v
                .into_iter()
                .take(days)
                .map(|(d, t)| json!({ "date": d.to_string(), "totals": t }))
                .collect::<Vec<_>>())
        }
        "get_recent" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            json!(aggregated.recent.iter().take(limit).collect::<Vec<_>>())
        }
        other => return err(id, -32602, format!("unknown tool: {other}")),
    };

    wrap_ok(id, payload)
}

fn pick_one(view: &AccountsView, name: Option<&str>) -> std::result::Result<Account, String> {
    if let Some(n) = name {
        return view
            .accounts
            .iter()
            .find(|a| a.name == n)
            .cloned()
            .ok_or_else(|| format!("unknown account: {n}"));
    }
    view.pick(None)
        .cloned()
        .ok_or_else(|| "no accounts discovered".to_string())
}

/// Aggregated multi-account view. When the scope is a single account this
/// matches the per-account [`Aggregate`] exactly; when it's multiple, totals
/// sum, `by_project` keys gain an `account/` prefix to stay unique, and
/// `recent` is interleaved chronologically across accounts.
struct Aggregated {
    all: UsageTotals,
    today: UsageTotals,
    this_week: UsageTotals,
    this_month: UsageTotals,
    by_model: BTreeMap<String, UsageTotals>,
    by_project: BTreeMap<String, UsageTotals>,
    by_day: BTreeMap<NaiveDate, UsageTotals>,
    recent: Vec<Value>,
    sessions_count: usize,
    projects_count: usize,
    accounts_in_scope: Vec<String>,
}

fn aggregate_scope(scope: &[Account]) -> Aggregated {
    let mut out = Aggregated {
        all: UsageTotals::default(),
        today: UsageTotals::default(),
        this_week: UsageTotals::default(),
        this_month: UsageTotals::default(),
        by_model: BTreeMap::new(),
        by_project: BTreeMap::new(),
        by_day: BTreeMap::new(),
        recent: Vec::new(),
        sessions_count: 0,
        projects_count: 0,
        accounts_in_scope: scope.iter().map(|a| a.name.clone()).collect(),
    };
    let single = scope.len() == 1;
    for account in scope {
        let agg: Aggregate = stats::load_and_aggregate_for(account).unwrap_or_default();
        merge_totals(&mut out.all, &agg.all);
        merge_totals(&mut out.today, &agg.today);
        merge_totals(&mut out.this_week, &agg.this_week);
        merge_totals(&mut out.this_month, &agg.this_month);
        for (k, v) in &agg.by_model {
            let entry = out.by_model.entry(k.clone()).or_default();
            merge_totals(entry, v);
        }
        for (k, v) in &agg.by_project {
            let key = if single { k.clone() } else { format!("{}/{}", account.name, k) };
            let entry = out.by_project.entry(key).or_default();
            merge_totals(entry, v);
        }
        for (d, v) in &agg.by_day {
            let entry = out.by_day.entry(*d).or_default();
            merge_totals(entry, v);
        }
        for r in &agg.recent {
            let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("account".to_string(), json!(account.name));
            }
            out.recent.push(v);
        }
        out.sessions_count += agg.sessions_count;
        out.projects_count += agg.projects_count;
    }
    // Re-sort the interleaved recent list by timestamp desc, keep up to 200.
    out.recent.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|x| x.as_str()).unwrap_or("");
        let tb = b.get("timestamp").and_then(|x| x.as_str()).unwrap_or("");
        tb.cmp(ta)
    });
    out.recent.truncate(200);
    out
}

fn merge_totals(dst: &mut UsageTotals, src: &UsageTotals) {
    dst.messages += src.messages;
    dst.input += src.input;
    dst.output += src.output;
    dst.cache_read += src.cache_read;
    dst.cache_write_5m += src.cache_write_5m;
    dst.cache_write_1h += src.cache_write_1h;
    dst.cost_usd += src.cost_usd;
}

fn wrap_ok(id: Option<Value>, payload: Value) -> Value {
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
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
