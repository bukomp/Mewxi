# claude-usage

A Rust tool for tracking, visualising, and exposing Claude Code usage stats
across one or many `CLAUDE_CONFIG_DIR` accounts. One binary, seven subcommands:

| Subcommand | What it does |
|------------|--------------|
| `claude-usage tui`    | Interactive full-screen dashboard with three views (all sessions / session detail / account detail). |
| `claude-usage status` | One-line ANSI-coloured summary for Claude Code's `statusLine`. Auto-detects which account owns the active transcript. |
| `claude-usage watch`  | Background daemon that keeps a `status-<account>.txt` per account hot. |
| `claude-usage setup`  | Wire `statusLine` into `~/.claude/settings.json` and (optionally) install a watcher service. |
| `claude-usage stop`   | Stop the watcher service (systemd on Linux, launchd on macOS); `--disable` also removes it from autostart. |
| `claude-usage dump`   | Dump the full per-account aggregate + live sessions as JSON. |
| `claude-usage mcp`    | Expose usage stats as an MCP server over stdio. |

Data comes from two sources:

1. **Local transcripts** — every assistant message in every JSONL under
   each account's `projects/` is parsed, tokens and dollars are summed,
   and the result is cached on disk keyed by `(mtime, size)` so re-scans
   are cheap. One cache file per account: `files-<slug>.json`.
2. **Claude Code's OAuth `/usage` endpoint** — the same source that powers
   the in-CLI `/usage` command and its status bar, giving the authoritative
   5-hour and 7-day utilisation windows. Cached for 60 s per account
   (`live-<slug>.json`) to stay cheap to call per keypress.

Everything is best-effort. If the OAuth call fails, the TUI falls back to
a local estimate. If the token store is unreadable, `--no-live` silences
the network entirely.

## Multi-account configuration

Most users run a single Claude Code account out of `~/.claude` and need no
config. If you split work and personal into separate dirs via
`CLAUDE_CONFIG_DIR` (e.g. `~/.claude-work` and `~/.claude-priv`),
`claude-usage` auto-discovers every `~/.claude*` directory that contains a
`projects/` subtree and treats each as one account.

For explicit control — friendly names, per-account token sources, custom
paths — create `~/.config/claude-usage/accounts.toml`:

```toml
default_account = "work"
ignored = ["default"]   # names to hide from every view (toggleable from view 4)

[[accounts]]
name = "work"
dir  = "~/.claude-work"
token_source = { keychain = "Claude Code-credentials" }

[[accounts]]
name = "priv"
dir  = "~/.claude-priv"
token_source = { keychain = "Claude Code-credentials-priv" }

[[accounts]]
name = "ci"
dir  = "~/.claude-ci"
token_source = { env = "CLAUDE_CI_TOKEN" }
```

`token_source` accepts one of:
- `{ env = "VARNAME" }` — read the bearer from an env var.
- `{ keychain = "service" }` — macOS `security find-generic-password -s <service>`.
- `{ file = "/path" }` — JSON file with `claudeAiOauth.accessToken`.

`ignored` is a list of account names you don't want claude-usage to
touch. Ignored accounts are still **discovered** so the setup view
(`4`) lists them and lets you press `i` to flip the flag — they just
don't appear in any other view, the `dump` output, MCP tool results,
or the statusLine. Toggling from the TUI rewrites this list back to
`accounts.toml` automatically; restart `claude-usage tui` after a
toggle for it to take effect across views 1/2/3.

If `accounts.toml` is missing, every entry defaults to the
`Claude Code-credentials` keychain service (or the existing
`~/.claude/.credentials.json` on Linux). The file is created on
demand the first time you press `i` in view 4.

---

## Install

```sh
cargo build --release
# Binary lands at ./target/release/claude-usage
```

No runtime dependencies beyond a working Rust toolchain at build time.
Live `/usage` fetching works on macOS and Linux; the OAuth Bearer token
is discovered in this order, first hit wins:

1. `CLAUDE_USAGE_OAUTH_TOKEN` env var (universal escape hatch: CI,
   remote shells without keychain access, Windows).
2. **macOS only** — the `Claude Code-credentials` keychain entry via
   `security find-generic-password`.
3. `~/.claude/.credentials.json` — the plaintext file (mode 0600) that
   Claude Code itself writes on Linux. Also acts as a fallback on
   macOS when the keychain is unavailable (sandboxed runs, no GUI).

Windows has no native-store integration yet; drop the credentials file
into `%USERPROFILE%\.claude\` or set the env var. If none of the above
is available, `--no-live` keeps every subcommand working against local
JSONL data only.

---

## The TUI

```sh
claude-usage tui
```

Four views:

| Key | View |
|-----|------|
| `1` | **All sessions** — per-account 5h / weekly / extra bars stacked at the top, plus a single table of every currently-running session across every account. |
| `2` | **Session detail** — the selected session's parent-account bars + this session's token breakdown, context %, and meta. |
| `3` | **Account detail** — the original single-pane dashboard scoped to the selected account (header, three gauges, burn rate, sparkline, efficiency, by-project table). |
| `4` | **Setup** — per-account statusLine wiring status + background watcher service status. Toggle individual rows with `s` / `w`, or press `a` to apply everything that's missing. The TUI drops you here automatically on first launch when anything is unwired; until then every other view shows a yellow `⚠ setup incomplete — press 4 to fix` banner at the top. |

Navigation: `Tab` / `↑` / `↓` cycle the selection inside the active
view; `Enter` from view 1 drills into the highlighted session. `q` /
`Esc` quits, `r` forces a reload + OAuth refetch.

The account-detail view auto-adapts to terminal width — above 100
columns you get a multi-column layout; below, a stacked one.

> **You don't need to run the `setup` subcommand by hand.** Launch
> `claude-usage tui` once; on first run it shows the setup view so
> you can wire `statusLine` for every account and install the watcher
> service with a single keypress (`a`).

### What each panel means

**Header** — session count, project count, all-time total cost, and a
live-status indicator (`live: fresh` / `cached` / `stale` / `off`).

**5h gauge** — the rolling 5-hour window that mirrors Anthropic's own
subscription accounting. Title says `(live)` when it comes from the OAuth
endpoint, `(estimate)` when computed locally from JSONL against
`CLAUDE_USAGE_5H_CAP_TOKENS` (default 11.5 M tokens, calibrated against
Max 5× at the time of writing — adjust for your plan). Reset time is the
clock-hour of the oldest message in the block plus 5 h.

**Weekly / Extra** — gauges sourced purely from the live endpoint. The
extra-usage gauge is only meaningful once you've incurred billed-extra
credit on a subscription plan; it hides gracefully when disabled.

**Burn rate (last 15m)** —
- `tok/hr`, `$/hr` are averaged over the last 15 minutes of the current
  5h block (not the last 15 min of wall clock). If nothing has happened
  in that window, both are zero by design: a stale burn rate would give
  you a bogus forever-scrolling ETA.
- `ETA to cap` projects the 5h cap hit using that burn rate, clamped to
  the end of the current 5h window. Reads `idle` when burn is zero, `—`
  when already over cap.
- `block cost`, `block msgs` are sums over the current 5h block.

**Daily tokens (14d)** — sparkline of daily token totals (local
calendar day). Caption shows peak, active-day average, and today.

**Efficiency** — cache-hit ratio (cache_read as a fraction of all
input-side tokens, all-time), projected 5h overage in USD for the
current block beyond the local cap, average cost per assistant
message, and the all-time total.

**By project** — per-project totals sorted by cost descending, with a
proportional bar relative to the top project. Project names are slugged
from the `~/.claude/projects/<slug>` directory (Claude Code encodes
paths by replacing `/` with `-`; we heuristically show the last two
segments joined).

### Colour thresholds

Percentages render green below 60, yellow 60–85, red at 85+. Cache-hit
ratio uses 40 / 70 as the yellow / green thresholds.

---

## `status` — the `statusLine` integration

```sh
claude-usage status        # reads Claude Code's stdin payload
```

Drop this into your Claude Code `statusLine` hook. Claude Code writes a
JSON payload to stdin containing `transcript_path` and `model.id`; we
use both to render:

- The 5h window (live if available, local estimate otherwise) with its
  reset time.
- An `extra` segment that promotes itself to the leading position
  (and hides the 5h percentage, keeping only the reset time) **once
  the current 5h window is at its cap**. Below the cap, `extra` is
  not shown in the statusLine even when credits have been spent
  earlier in the billing period — the TUI still surfaces that
  information on its own gauge.
- A `ctx` segment showing the current session's context utilisation
  against its cap. Cap is detected in order: `model.id` contains `[1m]`
  → 1 M; any message in the session had >200 K context → 1 M;
  `~/.claude/settings.json` model is `…[1m]` → 1 M; otherwise 200 K.

The 7d window is shown in the TUI but omitted from the statusLine to
keep the line short.

`claude-usage status` is fast enough to invoke per keypress — the
per-file JSONL cache (`files.json`) skips untouched files and the
live endpoint is served from `live.json` for 60 s between refreshes.
For heavier setups the optional `watch` daemon can pre-render the
line to disk (see below).

---

## `setup` — one-shot install

```sh
claude-usage setup              # wire statusLine + seed the cache
claude-usage setup --service    # also install a user service unit for `watch`
claude-usage setup --force      # overwrite an existing statusLine entry
```

Does the wiring described above for you:

1. Writes (or merges) a `statusLine` block into `~/.claude/settings.json`
   that runs `<binary> status` (so the stdin payload reaches the renderer
   and `ctx` can be shown). Idempotent: if the block already matches we
   leave it alone; if it points somewhere else we refuse to overwrite
   without `--force`. Other keys in `settings.json` are preserved.
2. Seeds the status cache with one render so the optional `watch`
   daemon has something on disk from the start.
3. With `--service`: installs a user-scope service unit and starts it.
   - **Linux:** `~/.config/systemd/user/claude-usage-watch.service`,
     enabled via `systemctl --user enable --now`.
   - **macOS:** `~/Library/LaunchAgents/com.claude-usage.watch.plist`,
     loaded via `launchctl load -w`.

The service unit captures the absolute path of the `claude-usage`
binary you ran `setup` from — if you move the binary later, re-run
`claude-usage setup --service` so the unit points at the new location.

## `stop` — stop the watcher

```sh
claude-usage stop              # stop the running service (will restart on login)
claude-usage stop --disable    # stop and prevent it from starting on login
```

Counterpart to `setup --service`. Maps to `systemctl --user stop` (Linux)
or `launchctl unload` (macOS). With `--disable`, also `systemctl --user
disable` / `launchctl unload -w` so the service does not come back on the
next login. Does nothing if no unit is installed — the unit file itself
is left on disk either way, so `setup --service` can bring it back.

## `watch` — background daemon

```sh
claude-usage watch         # runs forever
```

Spawns one `notify` watcher per account's `projects/` and writes a
per-account `status-<slug>.txt` plus a `status.txt` mirror of whichever
account was modified most recently. Atomic renames, per-account 500 ms
debounce, 15 s heartbeat.

- **Linux:** `$XDG_CACHE_HOME/claude-usage/` (defaults to
  `~/.cache/claude-usage/`).
- **macOS:** `~/Library/Caches/claude-usage/`.

A single-account statusLine that just `cat`s `status.txt` keeps working;
multi-account dashboards can point separately at `status-work.txt`,
`status-priv.txt`, etc.

---

## `mcp` — expose as an MCP server

```sh
claude-usage mcp           # stdio MCP
```

Speaks JSON-RPC 2.0 per the 2024-11-05 MCP protocol version. Every
data-returning tool accepts an optional `account` string; when omitted,
totals/breakdowns are summed across every configured account and
`by_project` keys are namespaced as `<account>/<project>` to stay
unique.

| Tool | Purpose |
|------|---------|
| `list_accounts`      | Every configured account with its directory. |
| `list_live_sessions` | Currently-active transcripts across all accounts (or `account`-filtered). |
| `get_totals`         | All-time / month / week / today totals plus session & project counts. |
| `get_today`          | Today's totals only. |
| `get_by_model`       | Totals grouped by model, sorted by cost. |
| `get_by_project`     | Totals grouped by project, sorted by cost. `limit` (default 50). |
| `get_by_day`         | Last `days` days (default 14), newest first. |
| `get_recent`         | Most recent assistant messages (`limit`, default 20). Each record gains an `account` field. |
| `get_live_usage`     | Live OAuth payload for one account. Defaults to `default_account`. |

Wire into Claude Code's MCP config as a stdio server pointed at the
built binary.

---

## `dump` — JSON

```sh
claude-usage dump | jq .
```

Emits

```json
{
  "generated_at": "...",
  "default_account": "...",
  "accounts": [
    { "name": "...", "dir": "...", "aggregate": {...}, "live": {...|null}, "live_sessions": [...] }
  ]
}
```

Handy for scripting your own analyses without recomputing everything.

---

## Environment variables & flags

| Setting | Effect |
|---------|--------|
| `--no-live` (global flag) / `CLAUDE_USAGE_NO_LIVE=<nonempty>` | Disable all calls to `api.anthropic.com/api/oauth/usage`. All panels fall back to local JSONL. |
| `CLAUDE_USAGE_5H_CAP_TOKENS`   | Override the local 5h token cap used by the `status` and TUI estimates. Default 11 500 000 (Max 5×). Pro ≈ 2 300 000, Max 20× ≈ 46 000 000. |

Caches live under `$XDG_CACHE_HOME/claude-usage/` (one per account, plus a mirror):

- `files-<slug>.json`   — per-account, per-file `(mtime, size, parsed_records)` cache so untouched JSONLs skip re-parsing.
- `live-<slug>.json`    — per-account last fetched OAuth payload + `fetched_at`.
- `status-<slug>.txt`   — per-account statusLine written by `watch`.
- `status.txt`          — mirror of whichever account was modified most recently (back-compat with single-account statusLine hooks).

---

## Architecture

### Module map

| Module | Role |
|--------|------|
| `main.rs`         | `clap` CLI parse, subcommand dispatch, stdin-payload decode for `status`, multi-account `dump`. |
| `accounts.rs`     | Discover accounts from `accounts.toml` and/or `~/.claude*` auto-discovery. `Account`, `TokenSource`, `AccountsView`. |
| `stats.rs`        | Per-account JSONL parsing, file cache, aggregation, 5h-block detection, pricing & context-cap heuristics. |
| `live_session.rs` | Detect currently-open Claude Code instances by reading `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` marker files (gated on PID liveness via `ps`). One row per running `claude` process; subagent / one-shot transcripts are excluded by canonical-path matching. Marker `status` drives `active` (busy) vs `idle`. |
| `live_usage.rs`   | Per-account HTTP call to `api.anthropic.com/api/oauth/usage`, cache, per-account 429 backoff. |
| `auth.rs`         | Read each account's OAuth Bearer over its configured `TokenSource` (env / keychain / file). |
| `tui/`            | Multi-view ratatui dashboard: `mod.rs` (event loop, ViewMode, AppState), `view_all.rs`, `view_session.rs`, `view_account.rs`, `widgets.rs`. |
| `watch.rs`        | Account-aware status-line renderer + per-account `notify` watcher fan-out. |
| `mcp.rs`          | JSON-RPC 2.0 MCP server; per-tool `account` filter and `list_accounts` / `list_live_sessions`. |

### Data flow (TUI)

```
┌───────────────────────┐     notify events    ┌──────────────┐
│ ~/.claude/projects/*  ├──────────────────────▶              │
└───────────────────────┘                      │   tui main   │
                                               │   loop       │
┌───────────────────────┐  background thread   │              │
│ OAuth /usage endpoint ├──────────────────────▶              │
└───────────────────────┘                      └──────┬───────┘
                                                      │ Aggregate + LiveUsage
                                                      ▼
                                               ratatui frame
```

The main loop reloads the on-disk transcripts at most every 500 ms when
a JSONL event has fired, and unconditionally every 5 s as a safety net.
The live poller runs on a separate thread with its own refresh cadence
(`REFRESH_INTERVAL = 60 s`, stretched to `BACKOFF_AFTER_429 = 120 s`
after a 429).

### How `UsageRecord` is built

For every assistant message in every JSONL:

1. Pull `input_tokens`, `output_tokens`, `cache_read_input_tokens`, plus
   `cache_creation.ephemeral_5m_input_tokens` and
   `cache_creation.ephemeral_1h_input_tokens` (falling back to a flat
   `cache_creation_input_tokens` as 5m when the nested shape is absent).
2. Drop rows with zero total tokens (stop events / metadata).
3. Price by model family using per-million-token rates (Opus / Sonnet /
   Haiku; unknown falls through to Sonnet rates). These rates are
   hard-coded constants in `stats::price_for` and approximate public
   list prices as of 2026-04.
4. Identify the record by `message.id`, falling back to the envelope
   `uuid`. Deduplication is by `message_id` across files (see below).

### The 5-hour block

Matches Anthropic's own accounting:

- A block starts at the **clock hour** of its first message
  (`floor_to_hour`).
- The block lasts exactly 5 h from that hour. Messages within
  `[start, start + 5h)` count toward the block.
- A gap of ≥5 h between consecutive messages ends a block; the next
  message starts a new one.
- A block is considered "current" only if `now < start + 5h`. Past
  blocks collapse to empty.

Overage cost is computed against `LOCAL_5H_CAP` proportionally: the
message that crosses the cap is billed by the fraction of its tokens
that landed beyond the cap.

### Dedup and project attribution

`scan_all` dedups records by `message_id` across all JSONL files. When
the same message appears in files under different project directories
(typical causes: `claude --resume`/`-c` launched from a different cwd,
forked sessions, or a project dir rename), only one copy wins — and
its file's directory determines its `project` field.

Iteration order is **stable**: files are sorted lexicographically
before dedup, so the winner is deterministic run to run. (Earlier
revisions iterated a `HashMap` directly, which flapped per-project
totals between scans because of Rust's randomised hasher.)

### Pricing assumptions

Hard-coded per-million-token prices (USD), approximate public list
prices as of 2026-04:

| Family       | input | output | cache write 5m | cache write 1h | cache read |
|--------------|------:|-------:|---------------:|---------------:|-----------:|
| Opus         | 15.00 | 75.00  | 18.75          | 30.00          | 1.50       |
| Sonnet / ?   |  3.00 | 15.00  |  3.75          |  6.00          | 0.30       |
| Haiku        |  1.00 |  5.00  |  1.25          |  2.00          | 0.10       |

Unknown model ids fall through to Sonnet rates. These are estimates;
authoritative billing comes from the live `/usage` endpoint.

---

## Troubleshooting

**"ETA to cap: idle" never changes.**
By design — burn rate is zero when no assistant message has landed in
the last 15 minutes. Idle flags an undefined ETA, not a stuck UI. The
value comes alive as soon as you start a session.

**Per-project totals used to flap between values with no Claude
running.**
Fixed in current `scan_all`: file iteration is sorted so dedup is
deterministic. If you still see flap, confirm that a stale daemon
isn't running an older binary (`ps aux | grep claude-usage` + restart
any `watch` / `tui` processes after a `cargo build --release`).

**5h gauge title says `(estimate)`, never `(live)`.**
One of: `--no-live` is set; the `CLAUDE_USAGE_NO_LIVE` env var is set;
no credential was found (`CLAUDE_USAGE_OAUTH_TOKEN` unset, no macOS
keychain entry, and `~/.claude/.credentials.json` missing or
unreadable); or the endpoint is rate-limited and there's no cache yet.
Run `claude-usage dump | jq .live` to see which branch you're in — a
`null` means no live data at all. On Linux, check
`ls -l ~/.claude/.credentials.json` — if Claude Code itself is logged
in, that file should exist with mode `0600`.

**5h local estimate disagrees with `(live)`.**
Expected when you're on a plan whose cap differs from `11 500 000`
tokens. Set `CLAUDE_USAGE_5H_CAP_TOKENS` to your plan's effective cap
(Pro ≈ 2.3 M, Max 20× ≈ 46 M).

**`security` prompts or returns a permission error (macOS).**
The first time claude-usage reads the keychain entry it may need your
approval. Open Keychain Access → login → search for
`Claude Code-credentials` → Access Control → allow `security`. If the
keychain entry is genuinely missing (e.g., sandboxed shell, no GUI
session), claude-usage transparently falls back to
`~/.claude/.credentials.json`.

**Live fetch fails on Linux.**
Verify the credentials file exists and is readable:
`ls -l ~/.claude/.credentials.json`. If Claude Code itself works but
the file is absent, log out and back in to Claude Code so it rewrites
the file. As a last resort, export `CLAUDE_USAGE_OAUTH_TOKEN` with the
Bearer token directly.
