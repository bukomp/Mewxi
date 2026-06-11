# TUI guide

`mewxi tui` opens an interactive ratatui dashboard. It updates live as
Claude Code writes JSONL.

## Views

| Key | View              | What you see                                                                |
| --- | ----------------- | --------------------------------------------------------------------------- |
| `1` | **Overview**      | Every account's 5h / weekly / extra-usage bars. Table of live sessions.     |
| `2` | **Session**       | Drill-down on the selected session: token breakdown, chat log, context.    |
| `3` | **Account**       | Single-account dashboard — gauges + per-model / per-project / per-day.     |
| `4` | **Setup**         | Same actions as `mewxi setup`, but with a checklist UI.                      |

## Keys

- `↑ ↓` — move selection in tables.
- `1` / `2` / `3` / `4` — switch view.
- `n` — create a new agent session (see below).
- `q` or `Esc` — quit.
- Dismiss the red error footer with the key the footer prints.

## Creating & driving sessions (beta)

The TUI can also **start** Claude Code sessions and drive them — spawn
`claude` in any project folder, type prompts, answer its permission
pickers, switch model/effort, run skills, and kill runaway sessions.
Press `n` anywhere to begin.

> **Beta:** session creation is a new feature and might have bugs.
> Everything it touches is a normal Claude Code process + transcript,
> so nothing is lost if Mewxi gets confused — but expect rough edges.

The full flow, keybinds, and caveats live in
[Agent sessions](sessions.md).

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
