<div align="center">

<img src="images/muxi.png" alt="muxi — the digi-cat mascot" width="220" />

# muxi

**MCP server + TUI for real-time Claude Code usage stats.**

One binary. Every account. No telemetry going anywhere but your terminal.

</div>

---

## Docs

- [Installation](docs/installation.md) — build from source, get the binary on your PATH.
- [Quick start](docs/quickstart.md) — five minutes from zero to a live status line.
- [TUI guide](docs/tui.md) — the four views, keybinds, what each pane means.
- [MCP server](docs/mcp.md) — wire muxi into Claude Code as a read-only data source.
- [Multi-account](docs/accounts.md) — point muxi at every `CLAUDE_CONFIG_DIR` you have.
- [Architecture](docs/architecture.md) — how the pieces fit, for the curious.
- [Roadmap](docs/roadmap.md) — what's coming next.

---

## What is this?

muxi reads the JSONL transcripts Claude Code already writes to disk, pairs
them with the same live `/usage` endpoint the CLI itself uses, and gives you:

- a **TUI dashboard** with 5-hour / weekly / extra-usage gauges, per-model
  and per-project breakdowns, and a live table of every running session
  across every account,
- a one-line **statusLine** for Claude Code (`muxi status`), kept hot by a
  small **watcher** daemon,
- an **MCP server** (`muxi mcp`) that exposes the same numbers as JSON-RPC
  tools so an agent can answer "how much have I spent today" without
  leaving the chat.

Read-only. No keys leave your machine. Pricing refreshes daily from
[LiteLLM's public table](https://github.com/BerriAI/litellm) and falls
back to baked-in rates when offline.

## Quick start

```bash
# 1. Build
cargo install --path .

# 2. Wire the statusLine + (optionally) install the watcher service
muxi setup --service

# 3. Open the dashboard
muxi tui
```

That's it — open Claude Code in another terminal and your status line will
show the current 5-hour window, your weekly budget, and the active
session's context size.

For MCP, see [docs/mcp.md](docs/mcp.md).

## Subcommands at a glance

| Command       | What it does                                                   |
| ------------- | -------------------------------------------------------------- |
| `muxi tui`    | Interactive ratatui dashboard.                                 |
| `muxi status` | Print one-line statusLine string (reads JSON from stdin).      |
| `muxi watch`  | Background daemon that keeps the status cache hot.             |
| `muxi mcp`    | JSON-RPC MCP server over stdio.                                |
| `muxi dump`   | Aggregate + live snapshot as JSON. Handy for scripts.          |
| `muxi setup`  | Wire statusLine into every discovered account, install service.|
| `muxi stop`   | Stop (and optionally disable) the watcher service.             |

---

<div align="center">

### 🐾 fun fact

> muxi is a **digi-cat**: nine lives, all of them spent in your terminal.
> She purrs in tokens, naps on JSONL, and judges your context window
> percentage from across the room. Feed her well — she remembers every
> 5-hour block you've ever burned, and she will mention it.

</div>
