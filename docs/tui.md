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
| `m` | **Mewxi rave**    | The Overview's data in Y2K-arcade dress: visualizer, streaks, screen shake. |

The TUI opens on the Overview; set `default_view` in
`~/.config/mewxi/accounts.toml` (`"overview"`, `"session"`, `"account"`,
`"config"`, or `"mewxi"`) to start somewhere else. First run still lands
on Config until setup is complete.

## Keys

- `↑ ↓` — move selection in tables.
- `1` / `2` / `3` / `4` — switch view.
- `n` — create a new agent session (see below).
- `?` — open the help modal. It lists **only** the shortcuts that
  actually work in the current view and state — a session mewxi didn't
  create won't show `Del kill` / `m model`. `?`, `Esc`, or `q` closes
  it. While a driven session's input is focused or claude is asking,
  `?` types into claude instead of opening help.
- `q` — quit.
- `Esc` — back to the sessions overview (view 1).
- Dismiss the red error footer with the key the footer prints.

Session-management shortcuts — `Del` (kill), `m` (model), `i` (type),
`/` (skill), `Shift-Tab` (mode), `Ctrl-C` / `Ctrl-D` — only act on
sessions **mewxi itself started** (via `n`, fresh or `--resume`). For an
observed session started in another terminal they're unavailable: `Del`
just explains that kill is mewxi-only, and `m` nudges you to drive the
session (`n`) first. mewxi never sends input to or tears down a process
it didn't spawn.

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

## Session view (2)

Drag across text in the Detail pane to select and copy it on release,
same as the chat-log pane; click a command part to copy just that part.

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
- **Mewxi view** — everything about the rave view (`m`): the agent
  visualizer (on/off), screen shake (`off` · `subtle` · `full`), the
  streak HUD (on/off), fx intensity (`chill` · `rave` · `insane`), and
  ascii style (`y2k` · `classic`). `Enter` toggles or cycles each row.
- **Status line** — open the **block composer** (`Enter` on "status line
  blocks") to reorder, toggle, add, and edit the pieces of the Claude
  Code status line, with a live preview. See
  [Status line](statusline.md).

Shortcuts: `a` fixes everything that's missing, `i` ignores/un-ignores
the selected account, `R` rescans, `Esc` goes back.

## Mewxi rave view (m)

Press `m` from any view except Session (where `m` is the model picker).
It shows the same accounts + live-sessions data as the Overview — the
same gauges, the same project-grouped table with sub-agent rows, the
same selection keys (`↑/↓` select, `Enter` open, `n` new, `Del` kill a
driven session, `r` refresh) — restyled as a purple-pink Y2K arcade
screen, plus:

- **Agent visualizer** — a strip of jumping columns along the bottom of
  the chrome column, one per running agent — sub-agents spawn their own
  bar right beside their parent session's, and the bars stretch to fill
  the strip's full width. Bars bounce high while an agent writes,
  runs, or edits, settle to a mid-level while it thinks or reads, and
  decay to an ember when it idles — a music visualizer that follows
  your agents instead of music.
- **Streak HUD** — an arcade band above the panels, tuned to reward
  *parallel* productivity: COMBO (agents working right now, sub-agents
  included — this is the score multiplier), STREAK (continuous time
  with ≥2 agents in parallel; a 15s grace bridges the gap between
  worker waves, but solo time doesn't build it), SCORE (the current
  run's score — each working agent earns points per second times the
  combo, so 4 parallel agents score 16× one, and streak tiers add +25%
  each; the run ends after 15s with no agents working, banking the
  score and resetting to zero), and BEST (the all-time high score,
  persisted across restarts — overtaking it flashes the HUD). On a tall enough
  terminal the values render in the same big pixel font as the
  headline; tighter screens fall back to a one-line HUD. Streak
  tier-ups and combo highs flash the HUD.
- **Score board** (`s`) — press `s` in the rave view to open a
  centered modal showing the current run's SCORE, the all-time BEST
  and BEST COMBO, the live COMBO and STREAK, and where the scores file
  lives. Below those stats it lists a HISTORY table of past finished
  runs, newest first, each row showing that run's score, peak combo,
  peak streak, and when it ended; the box is larger now to fit the
  table. `Esc`, `q`, or `s` closes it; other keys are swallowed while
  it's open.
- **Live ticker** — a scrolling one-line marquee under the pixel
  headline: ` ░ `-separated segments, one per session that needs
  attention or is working. Sessions waiting on a question or a
  permission dialog lead the line as `⚠ <project> NEEDS INPUT`; busy
  sessions follow as `<project> » <activity>`, appending ` +N⚡` and
  the lead sub-agent's live caption (truncated to 40 chars) when
  sub-agents are running underneath. At most 6 segments show, with the
  rest folded into a trailing `+k more`. When nothing's running it
  reads `agents.exe · idle — press n to spawn an agent`.
- **Screen shake** — a short pseudo-3D jolt (rows skew on a rolling
  wave) fires on the events worth feeling: an agent coming online, a
  session or sub-agent appearing or wrapping up, a burst of
  writing/editing/running kicking in, and — hardest of all — streak
  milestones. `subtle` keeps it to a 1-cell shimmy, `full` allows 3
  cells plus vertical jitter, `off` disables it.
- The animated cat mascot lives in the left chrome column under the
  headline and HUD, bobbing and colour-cycling faster the more agents
  are working.

All of it is configurable from the Config view's **Mewxi view** section,
or directly in `~/.config/mewxi/accounts.toml`:

| Key                  | Values                          | Default  |
| -------------------- | ------------------------------- | -------- |
| `mewxi_visualizer`   | `true` / `false`                | `true`   |
| `mewxi_shake`        | `off` / `subtle` / `full`       | `subtle` |
| `mewxi_streaks`      | `true` / `false`                | `true`   |
| `mewxi_fx_intensity` | `chill` / `rave` / `insane`     | `rave`   |
| `mewxi_ascii_style`  | `y2k` / `classic`               | `y2k`    |

`insane` adds a continuous low wobble while agents are active; `chill`
tones every effect down. `classic` swaps the big pixel-font headline for
a plain title but keeps the animations. The screen splits roughly 50/50
into a left chrome column (headline, ticker, streak HUD, mascot, and
the visualizer along its bottom edge) and a right data column (accounts
over the sessions table, full height), with the key hints on the very
bottom row like every other view. It degrades gracefully: narrow
terminals drop the chrome column entirely, short ones drop the
visualizer — the data panels always win.

Scores persist to `~/.local/state/mewxi/scores.json` (override the
base with `$XDG_STATE_HOME`), holding `best_score`, `best_combo`,
`current_score`, `current_combo`, `current_streak_secs`, and
`updated_at` (an RFC 3339 timestamp), plus a `history` array of
past-run objects `{score, peak_combo, peak_streak_secs, ended_at}`
(`ended_at` an RFC 3339 timestamp), newest first and capped at 50
entries. Older files written without the `history` key still load
fine — it defaults to empty. The file is written atomically
and debounced while the rave view is open, and flushed once more on
exit.

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

When a session delegates work with the Agent/Task tool, each sub-agent it
is **currently running** shows up as a dimmed, indented `↳` child row
beneath its session — labelled with the agent type and task, plus the
sub-agent's own model and live activity (it often runs a different model
than the main agent). The rows are display-only: `↑ ↓` still steps
session-to-session, and a sub-agent disappears once its delegation
returns. Detection lives in `src/subagents.rs`.

Each row also shows **price**, prefixed with the account's currency
symbol (`€`, `$`, `£`, …): `0.00` (dimmed) unless some of the row's
tokens were produced while the account was actually consuming
pay-per-use extra usage, in which case it shows an estimated
`~<sym>X.XX` (green). Attribution is causal, not a proportional smear:
mewxi records every observed increase in the account's extra-usage
spend (seen through its roughly 60-second usage polls) into a small
on-disk ledger, and splits each increment across the sessions that
were active during that interval. A session that ran entirely within
plan limits always reads `0.00`. The honest caveat: extra spend that
existed before mewxi started watching stays unattributed. The price is
in the account's extra-usage currency (e.g. EUR), not necessarily USD.
On wide terminals (width ≥ 98) two more columns appear, **5h%** and
**wk%**, showing that session's estimated share of the account's
5-hour and weekly limits — calibrated by scaling the session's local
cost proportion against the live utilization the OAuth `/usage`
endpoint reports for the account. (This is why 5h%/wk% now appear
before the in/out token columns at width ≥ 112 and the cache column at
width ≥ 121 — limit share outranks token-flow detail on narrow
screens.) The Session view (`2`) shows this same price, in the
account's currency, next to the token-value-at-API-rates figure,
relabelled "value".
