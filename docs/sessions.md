# Agent sessions

> **Beta.** Session creation and in-TUI agent control are new and may
> have bugs — overlay detection is heuristic, keybind handling varies
> by terminal, and edge cases around resume / session-id rotation are
> still being shaken out. If something misbehaves, kill the session
> (`Delete`) or quit the TUI; the underlying `claude` process and its
> transcript are normal Claude Code artifacts and survive either way.
> Bug reports with a debug-log excerpt (see
> [Troubleshooting](#troubleshooting)) are very welcome.

Beyond watching sessions, the TUI can **start and drive** Claude Code
itself: spawn `claude` in a hidden PTY, type prompts from a composer,
answer its permission prompts, switch model/effort, run skills, and
kill it — all without leaving Mewxi.

## Starting a session

Press **`n`** in any view to open the new-session modal. It has four
panes; `Tab` / `Shift-Tab` cycles between them, `Esc` closes the modal.

| Pane        | What it shows                                                                  |
| ----------- | ------------------------------------------------------------------------------ |
| **Account** | Every discovered account. `Enter` selects and advances.                        |
| **Recent**  | Up to 20 recently-used project folders for that account, freshest first, with session count and age. |
| **Folder**  | A directory browser with an editable path line. Typing after the last `/` fuzzy-filters entries; `Enter` drills into the highlighted entry. |
| **Resume**  | Sessions previously recorded in the browsed folder. The first row is always `+ Start a fresh session`; the rest resume an existing transcript. |

From any pane, **`.`** (period) spawns a fresh session in the current
directory immediately. Recent projects come from the account's
`<CLAUDE_CONFIG_DIR>/projects/` transcripts — Mewxi reads each
project's JSONL to recover the real working directory.

### What launching actually does

Mewxi spawns the `claude` binary attached to a 40×160 pseudo-terminal,
with the working directory you picked and `CLAUDE_CONFIG_DIR` set to
the chosen account (the default `~/.claude` account is left for
`claude` to discover on its own). Resuming passes
`--resume <session_id>`. If the account has opted into auto mode
(`skipAutoPermissionPrompt: true` in its settings), Mewxi passes
`--permission-mode auto`. Set `MEWXI_CLAUDE_BIN` to override which
binary is launched.

You're switched straight to the Session view with a "starting claude…"
placeholder. Once `claude` writes its live-session marker the
placeholder becomes a real session; if no marker appears within 15
seconds, the spawn is declared failed and you get an error instead.

## Driving a session

In the Session view (`2`), a bordered composer sits under the chat log.

| Key                  | Action                                                                   |
| -------------------- | ------------------------------------------------------------------------ |
| `i`                  | Focus the composer.                                                      |
| `Enter`              | Send the prompt (text first, then the submit keystroke a beat later).    |
| `Ctrl-E`             | Open your external editor (config `editor` field, then `$VISUAL` / `$EDITOR` / `vim`) pre-seeded with the composer text — for multi-line prompts. |
| `Ctrl-C`             | Soft-interrupt Claude (same as Esc in standalone Claude Code).           |
| `Ctrl-D`             | Send EOF — `claude` exits gracefully and the session ends.               |
| `Esc`                | Unfocus the composer.                                                    |

The composer supports readline-style editing (`Ctrl-A`, `Ctrl-U`,
`Ctrl-W`, arrows, …). After sending, focus returns to navigation by
default, so `↑ ↓` scroll the chat again; press `i` for the next prompt.

### Prompts and pickers

When Claude pops an interactive prompt — a `[y/N]` confirmation, a
numbered picker, a plan-approval dialog, an `AskUserQuestion` with
side-by-side previews — Mewxi detects it and renders it as a native
overlay titled `claude is asking`. While the overlay is up, your
keystrokes go straight to Claude: arrows move its cursor, numbers and
`y`/`n` pick options, `Enter` submits. Press **`F10`** to dismiss the
overlay without answering (it won't re-pop for the same prompt).

Detection is pattern-based and best-effort — an unusual prompt may not
be caught, in which case the session simply shows it in the chat log
and you can still answer through the composer.

### Model and effort

Press **`m`** to open the model & thinking picker: models on the left
(Opus / Sonnet / Haiku / Default), effort levels on the right where the
model supports them. `Tab` swaps columns, `↑ ↓` move.

- `Enter` — apply to **this session only** (sends `/model` and
  `/effort` to the running `claude`).
- `d` — apply *and* persist as the **account default** (writes `model`
  and `effortLevel` into the account's `settings.json`).

The current choice shows as a `[model:effort]` badge in the session
footer.

### Skills

Press **`/`** to open the skill picker. It lists the same skills
Claude Code itself would find — user (`<CLAUDE_CONFIG_DIR>/skills/`,
`commands/`), project (`.claude/skills/`, `.claude/commands/` walking
up to the repo root), installed plugins, and built-ins — each tagged
with its origin. Type to filter, `Enter` sends `/<name>` to the
session, `Esc` cancels.

### Permission mode

**`Shift-Tab`** cycles the permission mode (default → acceptEdits →
plan → auto when the account opted in), exactly like pressing Shift-Tab
inside Claude Code. The footer badge updates optimistically and is
confirmed when the next transcript record lands. Requires a terminal
that reports Shift-Tab (Mewxi enables the kitty keyboard protocol where
available).

### Stopping a session

Press **`Delete`** to kill the driven session. A confirmation modal
appears — `Enter` / `y` confirms, `Esc` / `n` cancels. For a graceful
end, prefer `Ctrl-D` in the composer.

`Delete` (and the other driving shortcuts — `i`, `m`, `/`, `Shift-Tab`,
`Ctrl-C`, `Ctrl-D`) only act on sessions **mewxi started itself**. For a
session discovered from another terminal, `Delete` declines with a note
that kill is mewxi-only and `m` nudges you to drive it first (`n`) —
mewxi never kills or sends input to a process it didn't spawn.

## Caveats and known rough edges

- **Overlay detection can miss or misfire.** It keys off visual
  markers in Claude's PTY screen. `F10` always dismisses a wrong
  overlay; undetected prompts can be answered via the composer.
- **Session IDs rotate** when you `/clear` or `/compact`; Mewxi
  re-pins to the new ID automatically, but a brief flicker of session
  metadata is possible.
- **Resume reuses Claude Code's own `--resume`** — anything Claude
  Code can't resume (deleted transcript, version mismatch), Mewxi
  can't either.
- **Defaults can go stale**: if you change model/effort in a
  standalone Claude Code session, Mewxi's cached account defaults
  catch up on the next TUI start.
- **Windows is untested** for PTY-driven sessions.

## Troubleshooting

Mewxi writes rotating debug logs to `<cache dir>/mewxi/logs/`
(`~/Library/Caches/mewxi/logs/` on macOS, `~/.cache/mewxi/logs/` on
Linux) — spawn attempts, overlay detection decisions, PTY events. Set
`MEWXI_LOG=0` to disable. When filing a bug about session creation,
include the tail of the newest `mewxi-*.log`.
