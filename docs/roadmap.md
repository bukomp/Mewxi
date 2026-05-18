# Roadmap

Where muxi is headed. Order is rough — not a commitment.

## Multi-CLI support

Today muxi is Claude-Code-shaped: it reads JSONL from `~/.claude*`,
talks to Anthropic's `/usage` endpoint, and wires into Claude Code's
`statusLine`. The plan is to broaden the data layer so the same TUI /
MCP / status surface works across every major coding agent.

- **Gemini CLI** — parse Gemini's transcript / session format, surface
  the same per-model / per-project breakdowns, and find a comparable
  live-usage signal (or estimate locally when none exists).
- **Codex** — parse OpenAI Codex CLI's session files, attribute spend
  per project, and feed the same gauges. Pricing already comes from
  LiteLLM, so the cost math is mostly free.

Each integration is a thin "provider" module behind the existing
`stats` / `live_session` traits. The UI doesn't change — accounts just
gain a provider tag.

## Single entry point: agent-teams orchestrator + builder

Beyond observability, muxi will grow a launcher for multi-agent
workflows:

- **Orchestrator** — one command that spawns a coordinated team of
  Claude / Gemini / Codex agents on the same task, routes their output,
  and surfaces progress in the TUI alongside the usage bars.
- **Builder** — a guided flow to compose a team from reusable roles
  (planner, implementer, reviewer, runner) without hand-editing JSON
  configs. Saves teams as named presets.

The goal: one binary for "see what my agents are doing" *and* "start a
new agent team," with cost accounting built in from day one.

## Smaller things

- Windows support (currently untested).
- Export aggregates as Prometheus / OpenMetrics for dashboarding.
- Per-project budgets with notifications when you cross them.

Suggestions welcome — open an issue.
