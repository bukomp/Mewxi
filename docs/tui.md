# TUI guide

`muxi tui` opens an interactive ratatui dashboard. It updates live as
Claude Code writes JSONL.

## Views

| Key | View              | What you see                                                                |
| --- | ----------------- | --------------------------------------------------------------------------- |
| `1` | **Overview**      | Every account's 5h / weekly / extra-usage bars. Table of live sessions.     |
| `2` | **Session**       | Drill-down on the selected session: token breakdown, chat log, context.    |
| `3` | **Account**       | Single-account dashboard — gauges + per-model / per-project / per-day.     |
| `4` | **Setup**         | Same actions as `muxi setup`, but with a checklist UI.                      |

## Keys

- `↑ ↓` — move selection in tables.
- `1` / `2` / `3` / `4` — switch view.
- `q` or `Esc` — quit.
- Dismiss the red error footer with the key the footer prints.

## What the gauges mean

- **5h** — Claude Code's rolling 5-hour usage window, against your plan's
  cap. The bar fills with what's confirmed via the OAuth `/usage`
  endpoint; an extrapolated estimate fills the rest from local JSONL.
- **Weekly** — your weekly Claude Code budget.
- **Extra** — pay-as-you-go usage outside the included caps. When this
  is non-zero it promotes itself in the status line.

## Live sessions

A row appears for every Claude Code instance that has a marker file at
`<CLAUDE_CONFIG_DIR>/sessions/<pid>.json`. Status (`busy` / `idle` /
`awaiting permission`) is read from the marker, not guessed from JSONL
mtimes. See `src/live_session.rs` for the gory details.
