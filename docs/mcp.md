# MCP server

`mewxi mcp` is a JSON-RPC 2.0 MCP server (protocol `2024-11-05`) that
runs over stdio. Read-only — it can't change your config, send tokens,
or modify transcripts.

## Wire it in

```bash
claude mcp add mewxi -- mewxi mcp
```

Or hand-edit your `~/.claude.json` (or per-account equivalent):

```json
{
  "mcpServers": {
    "mewxi": { "command": "mewxi", "args": ["mcp"] }
  }
}
```

## Tools

Every tool that reads usage takes an optional `account` string. Omit it
to aggregate across every discovered account.

| Tool                  | What it returns                                                      |
| --------------------- | -------------------------------------------------------------------- |
| `list_accounts`       | Every account Mewxi knows about, with directory + default flag.       |
| `list_live_sessions`  | Currently-active Claude Code conversations.                          |
| `get_totals`          | All-time / today / this week / this month, with USD cost.            |
| `get_today`           | Today only — same shape as `get_totals`.                             |
| `get_by_model`        | Totals grouped by model, sorted by cost.                             |
| `get_by_project`      | Totals grouped by project. `<account>/<project>` keys when global.   |
| `get_by_day`          | Daily totals for the last N days (default 14).                       |
| `get_recent`          | Most recent assistant messages, timestamped.                         |
| `get_live_usage`      | Authoritative 5h/weekly % from Anthropic's `/usage` endpoint.        |

## Example session

Ask Claude:

> "Using the Mewxi MCP, summarise my Claude spend this week and tell me
> which project ate the most tokens."

It will call `get_totals`, `get_by_project`, and respond in plain
English. No more guessing why the 5-hour bar is red.
