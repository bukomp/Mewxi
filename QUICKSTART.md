# claude-usage — Quickstart

Track Claude Code usage in your status line and a TUI dashboard.

## 1. Install the binary

```sh
cargo build --release
mkdir -p ~/.local/bin
cp target/release/claude-usage ~/.local/bin/
```

Make sure `~/.local/bin` is on your `PATH` (add to `~/.zshrc` / `~/.bashrc` if not):

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Verify:

```sh
claude-usage --help
```

## 2. Set up the status line + background watcher

```sh
claude-usage setup --service
```

That does three things:

- Wires the `statusLine` in `~/.claude/settings.json` to call `claude-usage status`.
- Seeds the cache with one render so the status line is populated immediately.
- Installs and starts a user service (`launchd` on macOS, `systemd --user` on Linux) that runs `claude-usage watch` and keeps the status line fresh.

Restart Claude Code — the status line now shows your usage.

## 3. Commands you'll actually use

| Command                    | What it does                                             |
|----------------------------|----------------------------------------------------------|
| `claude-usage tui`         | Full-screen dashboard. `q` quits, `r` reloads.           |
| `claude-usage status`      | One-line summary (called by Claude Code).                |
| `claude-usage setup`       | Wire the status line (add `--service` for watcher).      |
| `claude-usage stop`        | Stop the watcher (`--disable` also prevents autostart).  |
| `claude-usage dump \| jq`  | Dump everything as JSON.                                 |

Full reference: [README.md](./README.md).

## 4. How it works (30 seconds)

- Parses every assistant message in `~/.claude/projects/*.jsonl` and caches the result so re-scans are cheap.
- Fetches the live 5h / weekly window from Claude's OAuth `/usage` endpoint (same source as `/usage` in Claude Code).
- The `watch` daemon re-renders the status line on every file change and writes it to a cache file; Claude Code reads that file.

## 5. Status stopped updating?

1. Check the service is running:
   - macOS: `launchctl list | grep claude-usage`
   - Linux: `systemctl --user status claude-usage-watch`
2. Restart it:

   ```sh
   claude-usage stop
   claude-usage setup --service
   ```

3. Moved the binary after setup? Re-run `claude-usage setup --service` so the service points at the new path.
4. Still stale? Run `claude-usage status` manually — if that works, the watcher is the problem; if it errors, check your auth (see the "Live fetch fails" section of `README.md`).
