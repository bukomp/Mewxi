# TUI guide

`mewxi tui` opens an interactive ratatui dashboard. It updates live as
Claude Code writes JSONL.

## Views

| Key | View              | What you see                                                                |
| --- | ----------------- | --------------------------------------------------------------------------- |
| `1` | **Overview**      | Every account's 5h / weekly / extra-usage bars. Table of live sessions.     |
| `2` | **Session**       | Drill-down on the selected session: token breakdown, chat log, context.    |
| `3` | **Account**       | Single-account dashboard — gauges + per-model / per-project / per-day.     |
| `4` | **Config**        | statusLine wiring, watcher service, self-update channel, preferences.       |

The TUI opens on the Overview; set `default_view` in
`~/.config/mewxi/accounts.toml` (`"overview"`, `"session"`, `"account"`,
or `"config"`) to start somewhere else. First run still lands on Config
until setup is complete.

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

## Config view (4)

One navigable list, grouped into sections. `↑/↓` selects a row, `Enter`
performs that row's action — the hint box under the list always says
what `Enter` will do before you press it.

- **Claude Code integration** — per-account statusLine wiring and the
  background watcher service.
- **Updates** — the self-update channel (`release` follows version
  tags, `dev` follows the main branch), whether the TUI asks about
  updates on startup, where updates clone + build (the OS temp dir by
  default), and a check/install row.
- **Preferences** — TUI behaviour toggles.

Shortcuts: `a` fixes everything that's missing, `i` ignores/un-ignores
the selected account, `R` rescans, `Esc` goes back.

## Updates

On startup the TUI checks the source checkout's git remote in the
background; if something newer exists it asks before installing
(`Enter` updates, `Esc` postpones, `d` stops the startup question).
Installing clones the target ref into a throwaway folder under the OS
temp dir (configurable via the Config view's "update build dir" row or
`update_build_dir` in `accounts.toml`), rebuilds it via
`cargo install --path … --force`, deletes the clone, and restarts —
your source checkout is never touched.
`mewxi update` does the same from the CLI; `mewxi update --check` only
reports. While an update is pending, the Claude Code statusline also
shows a small `⬆ mewxi update` notice.

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
