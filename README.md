<div align="center">

<img src="images/mewxi.png" alt="Mewxi — the digi-cat mascot" width="220" />

# Mewxi

**MCP server + TUI for real-time coding-agent usage stats.**

One binary. Every account. No telemetry going anywhere but your terminal.

> Today Mewxi reads Claude Code. Gemini CLI and Codex are on the [roadmap](docs/roadmap.md).

</div>

---

## Docs

- [Installation](docs/installation.md) — build from source, get the binary on your PATH.
- [Quick start](docs/quickstart.md) — five minutes from zero to a live status line.
- [TUI guide](docs/tui.md) — the four views, keybinds, what each pane means.
- [MCP server](docs/mcp.md) — wire Mewxi into Claude Code as a read-only data source.
- [Multi-account](docs/accounts.md) — point Mewxi at every `CLAUDE_CONFIG_DIR` you have.
- [Architecture](docs/architecture.md) — how the pieces fit, for the curious.
- [Roadmap](docs/roadmap.md) — what's coming next.

---

## What is this?

Mewxi reads the JSONL transcripts Claude Code already writes to disk, pairs
them with the same live `/usage` endpoint the CLI itself uses, and gives you:

- a **TUI dashboard** with 5-hour / weekly / extra-usage gauges, per-model
  and per-project breakdowns, and a live table of every running session
  across every account,
- a one-line **statusLine** for Claude Code (`mewxi status`), kept hot by a
  small **watcher** daemon,
- an **MCP server** (`mewxi mcp`) that exposes the same numbers as JSON-RPC
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
mewxi setup --service

# 3. Open the dashboard
mewxi tui
```

That's it — open Claude Code in another terminal and your status line will
show the current 5-hour window, your weekly budget, and the active
session's context size.

For MCP, see [docs/mcp.md](docs/mcp.md).

## Subcommands at a glance

| Command       | What it does                                                   |
| ------------- | -------------------------------------------------------------- |
| `mewxi tui`    | Interactive ratatui dashboard.                                 |
| `mewxi status` | Print one-line statusLine string (reads JSON from stdin).      |
| `mewxi watch`  | Background daemon that keeps the status cache hot.             |
| `mewxi mcp`    | JSON-RPC MCP server over stdio.                                |
| `mewxi dump`   | Aggregate + live snapshot as JSON. Handy for scripts.          |
| `mewxi setup`  | Wire statusLine into every discovered account, install service.|
| `mewxi stop`   | Stop (and optionally disable) the watcher service.             |

---

<div align="center">

### 🐾 fun fact

> Mewxi is a **digi-cat**. She has as many lives as you have agents open,
> and unlike other cats, she lives them all at once.

</div>
