# Installation

Mewxi is a single Rust binary. No daemon to install, no Python to manage.

## From source

```bash
git clone <repo> mewxi && cd mewxi
cargo install --path .
```

The binary lands in `~/.cargo/bin/mewxi`. Make sure that's on your `PATH`.

## Requirements

- Rust 1.75+ (2021 edition).
- An existing Claude Code install — Mewxi reads from `~/.claude*/projects/`
  and the OAuth credentials those directories already hold.
- Linux or macOS. Windows isn't tested.

## Updating

Mewxi updates itself from the same git checkout it was installed from:

```bash
mewxi update          # fetch, fast-forward, rebuild
mewxi update --check  # just report
```

The TUI also checks on startup and asks before installing. Two
channels, set via `update_channel` in `~/.config/mewxi/accounts.toml`
or from the TUI's Config view (`4`):

- `release` (default) — follow version tags (`v0.2.0`).
- `dev` — follow the main branch.

If the checkout moved after the binary was built, point
`update_repo_dir` in `accounts.toml` at its new location. Set
`update_prompt = false` to silence the startup question.

## Optional: watcher service

```bash
mewxi setup --service
```

Installs a user-scope `systemd` unit (Linux) or `launchd` agent (macOS)
that runs `mewxi watch` at login. Without it, the statusLine still works —
it just recomputes on every Claude Code tick instead of reading a cache.

Uninstall with `mewxi stop --disable`.
