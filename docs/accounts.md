# Multi-account

An "account" in Mewxi is one `CLAUDE_CONFIG_DIR` — a directory with its
own `projects/` JSONL subtree and its own credentials. People with a
work/personal split (`claude-work`, `claude-priv`) point each shell at a
different dir; Mewxi sees all of them at once.

## Discovery order

1. **`~/.config/mewxi/accounts.toml`** — explicit config, wins when present.
2. **Auto-discovery** — every `~/.claude*` directory containing `projects/`.
3. **`CLAUDE_CONFIG_DIR` env var** — added if not already in the list.

Dedup by canonicalised path, sort by name. Stable iteration order.

## Example `accounts.toml`

```toml
default_account = "work"

# View the TUI opens on: "overview" (default), "session", "account",
# or "config" — the view's number key ("1"–"4") works too.
default_view = "overview"

# Live /usage polling. Raise these if the endpoint rate-limits you
# (429s in the TUI / statusline). Both are floored at 10s; the
# statusline picks up edits immediately, a running TUI on restart.
live_refresh_interval_secs = 300  # min seconds between HTTP probes (default 60)
live_backoff_secs = 600           # wait after a 429/401/403 (default 120)

[[account]]
name = "work"
dir  = "/Users/me/.claude-work"

[[account]]
name = "priv"
dir  = "/Users/me/.claude-priv"
```

If you set up Claude Code the normal way (single `~/.claude`) you don't
need this file. Mewxi finds it automatically.

## Auth

Per account, in order:

1. `MEWXI_OAUTH_TOKEN` env var — universal escape hatch.
2. Account's configured `TokenSource`: env var, macOS keychain, or file.

If every source fails, the error message lists exactly what was tried —
so you can fix it instead of guessing.

## What you see in the UI

When more than one account is configured, the statusLine is prefixed
with the account name in brackets (`[priv] 5h …`) and the TUI's overview
view shows one row of gauges per account. With a single account the
prefix is dropped.
