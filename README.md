# claude-usage

A Rust tool for tracking, visualising, and exposing Claude Code usage stats.
One binary, five subcommands:

| Subcommand | What it does |
|------------|--------------|
| `claude-usage tui`    | Interactive full-screen dashboard that updates as session files change. |
| `claude-usage status` | One-line ANSI-coloured summary for Claude Code's `statusLine`. |
| `claude-usage watch`  | Background daemon that keeps the `status` output cached and hot. |
| `claude-usage dump`   | Dump the full aggregate as JSON (for scripts / debugging). |
| `claude-usage mcp`    | Expose usage stats as an MCP server over stdio. |

Data comes from two sources:

1. **Local transcripts** — every assistant message in every JSONL under
   `~/.claude/projects/` is parsed, tokens and dollars are summed, and the
   result is cached on disk keyed by `(mtime, size)` so re-scans are cheap.
2. **Claude Code's OAuth `/usage` endpoint** — the same source that powers
   the in-CLI `/usage` command and its status bar, giving the authoritative
   5-hour and 7-day utilisation windows. Cached for 60 s to stay cheap to
   call per keypress.

Everything is best-effort. If the OAuth call fails, the TUI falls back to
a local estimate. If the token store is unreadable, `--no-live` silences
the network entirely.

---

## Install

```sh
cargo build --release
# Binary lands at ./target/release/claude-usage
```

No runtime dependencies beyond a working Rust toolchain at build time.
macOS is the only platform where live `/usage` fetching is wired up
(requires access to the `Claude Code-credentials` keychain entry via
`security find-generic-password`). Other platforms still work in
`--no-live` mode using local JSONL data only.

---

## The TUI

```sh
claude-usage tui
```

Keys: `q` / `Esc` to quit, `r` to force reload + live refetch.

The layout auto-adapts to terminal width. Above 100 columns you get a
multi-column layout; below, a stacked one.

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

- The 5h window (live if available, local estimate otherwise).
- The 7d window (live only; omitted if not available).
- An `extra` segment when subscription credits are being actively
  burned — this promotes itself to leading position and hides the 5h
  percentage (but keeps the reset time).
- A `ctx` segment showing the current session's context utilisation
  against its cap. Cap is detected in order: `model.id` contains `[1m]`
  → 1 M; any message in the session had >200 K context → 1 M;
  `~/.claude/settings.json` model is `…[1m]` → 1 M; otherwise 200 K.

To avoid re-parsing every session file on every keypress, run a
background watcher (see next section).

---

## `watch` — background daemon

```sh
claude-usage watch         # runs forever
```

Watches `~/.claude/projects/` for JSONL changes via `notify`, re-renders
the status line, and writes it atomically to
`$XDG_CACHE_HOME/claude-usage/status.txt` (i.e. macOS:
`~/Library/Caches/claude-usage/status.txt`).

- Coalesces event bursts; writes at most once per 500 ms.
- Writes a heartbeat every 15 s even when idle so the cache never goes
  stale during low-activity periods.
- If the cache file is deleted out from under it, recreates on next
  tick.

Typical setup: wire `statusLine` to `cat ~/Library/Caches/claude-usage/status.txt`
and run `claude-usage watch` as a launchd agent or equivalent.

---

## `mcp` — expose as an MCP server

```sh
claude-usage mcp           # stdio MCP
```

Speaks JSON-RPC 2.0 per the 2024-11-05 MCP protocol version. Tools
exposed:

| Tool | Purpose |
|------|---------|
| `get_totals`     | All-time / month / week / today totals plus session & project counts. |
| `get_today`      | Today's totals only. |
| `get_by_model`   | Totals grouped by model, sorted by cost. |
| `get_by_project` | Totals grouped by project, sorted by cost. `limit` (default 50). |
| `get_by_day`     | Last `days` days (default 14), newest first. |
| `get_recent`     | Most recent assistant messages (`limit`, default 20). |
| `get_live_usage` | Raw live payload from the OAuth endpoint. Returns `{unavailable: true, ...}` when no credential / rate-limited / `--no-live`. |

Wire into Claude Code's MCP config as a stdio server pointed at the
built binary.

---

## `dump` — JSON

```sh
claude-usage dump | jq .
```

Emits `{ "aggregate": {...}, "live": {...|null} }`. Handy for scripting
your own analyses without recomputing everything.

---

## Environment variables & flags

| Setting | Effect |
|---------|--------|
| `--no-live` (global flag) / `CLAUDE_USAGE_NO_LIVE=<nonempty>` | Disable all calls to `api.anthropic.com/api/oauth/usage`. All panels fall back to local JSONL. |
| `CLAUDE_USAGE_5H_CAP_TOKENS`   | Override the local 5h token cap used by the `status` and TUI estimates. Default 11 500 000 (Max 5×). Pro ≈ 2 300 000, Max 20× ≈ 46 000 000. |

Caches live under `$XDG_CACHE_HOME/claude-usage/`:

- `files.json`   — per-file `(mtime, size, parsed_records)` cache so
  untouched JSONLs skip re-parsing.
- `live.json`    — last fetched OAuth payload + `fetched_at`.
- `status.txt`   — the last statusLine string written by `watch`.

---

## Architecture

### Module map

| Module | Role |
|--------|------|
| `main.rs`        | `clap` CLI parse, subcommand dispatch, stdin-payload decode for `status`. |
| `stats.rs`       | JSONL parsing, per-file cache, aggregation, 5h-block detection, pricing & context-cap heuristics. **The heart of the app.** |
| `live_usage.rs`  | HTTP call to `api.anthropic.com/api/oauth/usage`, schema, on-disk cache, 429 backoff + log dedupe. |
| `auth.rs`        | Read the OAuth Bearer token from the OS credential store (macOS only so far). |
| `tui.rs`         | Ratatui layout, rendering, keybindings, `notify`-driven reload loop, live-poller thread. |
| `watch.rs`       | Status-line string formatting and the `watch` daemon loop. |
| `mcp.rs`         | JSON-RPC 2.0 MCP server over stdio. |

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
you're not on macOS; you don't have a Claude Code subscription token in
the keychain; or the endpoint is rate-limited and there's no cache yet.
Run `claude-usage dump | jq .live` to see which branch you're in — a
`null` means no live data at all.

**5h local estimate disagrees with `(live)`.**
Expected when you're on a plan whose cap differs from `11 500 000`
tokens. Set `CLAUDE_USAGE_5H_CAP_TOKENS` to your plan's effective cap
(Pro ≈ 2.3 M, Max 20× ≈ 46 M).

**`security` prompts or returns a permission error.**
The first time claude-usage reads the keychain entry it may need your
approval. Open Keychain Access → login → search for
`Claude Code-credentials` → Access Control → allow `security`.
