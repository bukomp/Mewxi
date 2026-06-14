# Status line

The one-line string Mewxi prints inside every Claude Code session
(`mewxi status`) is composed from **blocks** — small, reorderable pieces
you can toggle, recolor, add to, and remove. With no configuration it
looks exactly as it always has; customize it only if you want to.

A typical default line:

```
[priv] Opus 4.8 · think:high | 5h 12.3% (live) → reset 14:30 (42m) | ctx 45% (90k/200k)
```

Each segment above is a block: `prefix`, `model`, `five_hour`, `reset`,
`ctx` — plus `hint`/`update` nudges and an `extra` (pay-as-you-go) meter
that appears when you're billing past the 5h cap.

## The composer (easiest way)

Open `mewxi tui` → Config view (`4`) → select **status line blocks** →
`Enter`. You get a list of every known block with a **live preview** at
the bottom:

| Key            | Action                                            |
| -------------- | ------------------------------------------------- |
| `↑` / `↓`      | move the selection                                |
| `Shift+↑/↓` or `J` / `K` | move the selected block up / down (reorder) |
| `Space`        | enable / disable the selected block               |
| `e`            | edit the selected block's `.toml` in `$EDITOR`    |
| `n`            | create a new block (type an id, opens `$EDITOR`)  |
| `Enter`        | save the composition                              |
| `Esc`          | cancel without saving                             |

Saving writes the order + enabled flags to `status_blocks` in
`~/.config/mewxi/accounts.toml`. Editing a built-in block with `e` copies
it into your blocks folder first, so the original is never lost.

## How blocks are stored

- **Built-in defaults** ship inside the binary (their source lives in the
  repo's `blocks/` folder). They work with zero config.
- **Your blocks** live in `status_blocks_dir` — default
  `~/.config/mewxi/blocks/`. A file there **overrides** a built-in block
  of the same id, or **adds** a brand-new one. Change the folder with
  `status_blocks_dir = "~/my/blocks"` in `accounts.toml`.
- **The composition** (which blocks render, and in what order) is the
  `status_blocks` array in `accounts.toml`:

  ```toml
  status_blocks_dir = "~/.config/mewxi/blocks"

  [[status_blocks]]
  id = "model"
  enabled = true

  [[status_blocks]]
  id = "git_branch"   # a custom block of yours
  enabled = true

  [[status_blocks]]
  id = "five_hour"
  enabled = false     # listed but hidden
  ```

  When `status_blocks` is present it is the source of truth: built-in
  blocks you leave out stay hidden (the composer still lists them so you
  can re-add them). A new `.toml` you drop in your folder that isn't yet
  listed is appended automatically. With no `status_blocks` at all, the
  built-in order is used and the line is byte-for-byte the classic one.

## Writing a block

A block is a TOML file; the filename stem is its default `id`.

### Template blocks

```toml
# blocks/ctx.toml
label = "context"          # shown in the composer
when = "ctx_present"       # visibility condition (optional)
template = " <grey>|</grey> <cyan>ctx</cyan> {ctx_pct} ({ctx_cur}/{ctx_cap})"
```

- `{field}` interpolates a value. Many fields are **self-coloring**
  (percentages turn green/yellow/red; whole-segment fields carry their own
  colors) — don't wrap those in a color tag.
- `<color>…</color>` colors literal text. Colors: `cyan`, `grey`/`gray`,
  `yellow`, `magenta`, `red`, `green`, `blue`, `white`. Tags may nest.

**Available fields**

| Field | Meaning |
| ----- | ------- |
| `{account}` | account name (plain) |
| `{model}` | compacted model name, e.g. `Opus 4.8` |
| `{think}` | ` · think:LVL` when extended thinking is on, else empty |
| `{five_h_segment}` | the whole 5h meter (label + % + live/stale/est tag) |
| `{reset_segment}` | the whole ` → reset HH:MM (Nm)` run |
| `{ctx_pct}` `{ctx_cur}` `{ctx_cap}` | context %, used tokens, cap |
| `{extra_pct}` `{extra_amounts}` | extra-usage % and `($used/$limit)` |
| `{update_segment}` `{hint_segment}` | the update / setup-incomplete nudges |

**`when` conditions** (prefix with `!` to negate, e.g. `!billing_extra`):
`always`, `multi_account`, `model_present`, `five_h_visible`,
`billing_extra`, `reset_present`, `ctx_present`, `update_available`,
`setup_incomplete`. An unknown condition keeps the block hidden.

### Command blocks

Run a shell command and show its output — handy for the current git
branch, working directory, hostname, etc.

```toml
# blocks/git_branch.toml
type = "command"
label = "git branch"
when = "always"
command = "git rev-parse --abbrev-ref HEAD"
color = "green"        # optional; wraps the whole output
timeout_ms = 300       # optional; clamped to 50–2000, default 300
```

Behavior and limits:

- Runs via the shell (`sh -c` / `cmd /C`) on each refresh, **bounded** by
  `timeout_ms` so a slow command can never stall the status line.
- Only the **first line** of stdout is used, with control characters /
  ANSI stripped and the result truncated — output can't break the line.
- Results are cached briefly (~5s) so repeated refreshes don't re-spawn.

> **Security:** command blocks are your own local config — the same trust
> level as the `$EDITOR` Mewxi already launches. Field values are never
> interpolated into the command, so there's no injection surface beyond
> what you write yourself. Don't copy command blocks from untrusted
> sources without reading them.

## Tips

- The line is recomposed on every refresh, so edits take effect on the
  next status update — no restart needed.
- To get back to the stock line, delete `status_blocks` (and any
  overrides in your blocks folder).
- A malformed block file is skipped (not fatal) — the rest of the line
  still renders.
