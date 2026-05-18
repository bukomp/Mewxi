# Quick start

Five minutes from clone to a live status line.

## 1. Install

```bash
cargo install --path .
```

See [installation.md](installation.md) for prerequisites.

## 2. Wire it in

```bash
muxi setup --service
```

This does two things:

- Adds a `statusLine` entry to every Claude Code `settings.json` it finds
  (one per `CLAUDE_CONFIG_DIR`).
- Installs and starts a user-scope watcher service that keeps the status
  cache hot.

Re-run any time — it's idempotent. `--force` overwrites a non-muxi
statusLine. Drop `--service` if you only want the wiring.

## 3. Open the dashboard

```bash
muxi tui
```

Keys: `1` overview, `2` session detail, `3` account detail, `4` setup,
`q` quit. Arrow keys move the selection. See [tui.md](tui.md).

## 4. (Optional) wire MCP

```bash
claude mcp add muxi -- muxi mcp
```

Now ask Claude things like *"what's my 5-hour usage right now?"* — see
[mcp.md](mcp.md) for the full tool list.

## Sanity checks

- `muxi dump` — prints the full aggregate as JSON. If this works,
  everything else will.
- `muxi status < /dev/null` — prints the same line your statusLine shows.
- `muxi setup` with no flags — re-runs setup and tells you what's wired
  and what isn't.
