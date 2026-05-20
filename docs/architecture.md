# Architecture

A short tour of the codebase for anyone who wants to hack on it.

## Data sources

- **JSONL transcripts** — Claude Code writes one line per assistant
  message under `<CLAUDE_CONFIG_DIR>/projects/<project>/<session>.jsonl`.
  Source of truth for historical usage. Parsed and cached on disk by
  `(mtime, size)` in `$XDG_CACHE_HOME/mewxi/`.
- **OAuth `/usage` endpoint** — the same undocumented endpoint Claude
  Code uses for its in-CLI `/usage` command. Source of truth for the
  authoritative 5h / weekly bars.
- **Session markers** — `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` files
  written by every running Claude Code instance. Ground truth for "is
  this session live."

## Modules

| File              | Role                                                       |
| ----------------- | ---------------------------------------------------------- |
| `main.rs`         | CLI parsing, hook handler, subcommand dispatch.            |
| `accounts.rs`     | Discovery + config of every `CLAUDE_CONFIG_DIR`.           |
| `auth.rs`         | Resolve OAuth tokens per account.                          |
| `stats.rs`        | JSONL parsing, dedup, aggregation, on-disk cache.          |
| `pricing.rs`      | LiteLLM-backed pricing with offline fallback.              |
| `live_usage.rs`   | OAuth `/usage` client.                                     |
| `live_session.rs` | PID-marker driven live-session scan.                       |
| `chat_log.rs`     | Render JSONL into a flat chat view for the session pane.   |
| `watch.rs`        | Status renderer + background watcher daemon.               |
| `setup.rs`        | StatusLine wiring + systemd/launchd service install.       |
| `mcp.rs`          | JSON-RPC MCP server (stdio).                               |
| `tui/`            | The ratatui dashboard. One file per view.                  |

## Concurrency in the TUI

- One `notify` watcher per account `projects/` dir, fanned into a single
  mpsc.
- One live-usage poller per account, staggered to spread network load.
- The event loop drains channels each tick, debounces dirty reloads to
  ≥500ms per account, rescans live sessions, redraws.

No async runtime in the TUI — just plain threads + channels. The MCP
server uses tokio because it's a stdio JSON-RPC loop.

## Caching

Per-file parse results live in `$XDG_CACHE_HOME/mewxi/files-<slug>.json`,
one cache per account. Invalidated on `(mtime, size)` mismatch. Pricing
cache is in the same dir, refreshed every 24h.
