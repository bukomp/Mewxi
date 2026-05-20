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

## Optional: watcher service

```bash
mewxi setup --service
```

Installs a user-scope `systemd` unit (Linux) or `launchd` agent (macOS)
that runs `mewxi watch` at login. Without it, the statusLine still works —
it just recomputes on every Claude Code tick instead of reading a cache.

Uninstall with `mewxi stop --disable`.
