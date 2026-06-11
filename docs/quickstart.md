# Quick start

Five minutes from clone to a live status line.

## 1. Install

```bash
cargo install --path .
```

Or grab a prebuilt binary from
[Releases](https://github.com/bukomp/Mewxi/releases). See
[installation.md](installation.md) for prerequisites, prebuilt-binary
notes, and how self-update works.

## 2. Wire it in

```bash
mewxi setup --service
```

This does two things:

- Adds a `statusLine` entry to every Claude Code `settings.json` it finds
  (one per `CLAUDE_CONFIG_DIR`).
- Installs and starts a user-scope watcher service that keeps the status
  cache hot.

Re-run any time — it's idempotent. `--force` overwrites a non-mewxi
statusLine. Drop `--service` if you only want the wiring.

## 3. Open the dashboard

```bash
mewxi tui
```

Keys: `1` overview, `2` session detail, `3` account detail, `4` setup,
`q` quit. Arrow keys move the selection. See [tui.md](tui.md).

## 4. (Optional) wire MCP

```bash
claude mcp add mewxi -- mewxi mcp
```

Now ask Claude things like *"what's my 5-hour usage right now?"* — see
[mcp.md](mcp.md) for the full tool list.

## Sanity checks

- `mewxi dump` — prints the full aggregate as JSON. If this works,
  everything else will.
- `mewxi status < /dev/null` — prints the same line your statusLine shows.
- `mewxi setup` with no flags — re-runs setup and tells you what's wired
  and what isn't.
