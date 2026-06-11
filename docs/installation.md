# Installation

Mewxi is a single Rust binary. No daemon to install, no Python to manage.

Two ways to get it: grab a prebuilt binary from a release, or build
from source. Building from source is what the built-in self-updater
expects — see [Updating](#updating) for the trade-off.

## Prebuilt binaries

Every tagged release ships binaries for macOS (arm64 / x86_64) and
Linux (arm64 / x86_64), plus a `SHA256SUMS` file:
<https://github.com/bukomp/Mewxi/releases>

The repo is private, so the easiest download is the `gh` CLI:

```bash
# pick your target: aarch64-apple-darwin, x86_64-apple-darwin,
#                   aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu
gh release download --repo bukomp/Mewxi --pattern '*aarch64-apple-darwin*'
tar -xzf mewxi-v*-aarch64-apple-darwin.tar.gz
install -m 755 mewxi ~/.cargo/bin/   # or any dir on your PATH
```

On macOS, a binary downloaded through a browser carries the quarantine
attribute and Gatekeeper will refuse to run it; clear it with:

```bash
xattr -d com.apple.quarantine ~/.cargo/bin/mewxi
```

(`gh release download` and `curl` don't set the attribute, so this is
only needed for browser downloads.)

> Replacing an existing `mewxi` binary later? Don't `cp` over it in
> place — macOS's code-signing cache kills binaries overwritten at the
> same path. Copy to a temp name in the same directory and `mv` it
> over, or just delete the old one first. Mewxi's own self-updater
> does this for you.

## From source

```bash
git clone git@github.com:bukomp/Mewxi.git mewxi && cd mewxi
cargo install --path .
```

The binary lands in `~/.cargo/bin/mewxi`. Make sure that's on your
`PATH` — and that no stale copy of `mewxi` sits in a directory that
comes *earlier* on `PATH` (check with `which -a mewxi`), or updates
will never reach the binary you actually run.

## Requirements

- An existing Claude Code install — Mewxi reads from `~/.claude*/projects/`
  and the OAuth credentials those directories already hold.
- Linux or macOS. Windows isn't tested.
- Rust 1.75+ (2021 edition) — only when building from source or using
  the self-updater (which rebuilds with cargo).

## Updating

Mewxi updates itself from a git checkout of this repo:

```bash
mewxi update          # fetch, fast-forward, rebuild, replace the running binary
mewxi update --check  # just report
```

The TUI also checks on startup and asks before installing. Two
channels, set via `update_channel` in `~/.config/mewxi/accounts.toml`
or from the TUI's Config view (`4`):

- `release` (default) — follow version tags (`v1.0.1`).
- `dev` — follow the main branch.

The updater fast-forwards the checkout, rebuilds with
`cargo install --path . --force`, and — if the mewxi you're running
lives somewhere other than cargo's bin dir — installs over the running
binary too. It refuses to touch a checkout with uncommitted changes.

**If you installed a prebuilt binary**, the self-updater doesn't know
where the source lives (that path is baked in at build time, and for
release binaries it points at the CI runner). Either:

- clone the repo and point `update_repo_dir` in `accounts.toml` at it
  (you'll need Rust; from then on `mewxi update` works), or
- ignore self-update and download the next release the same way you
  got this one. The Claude Code statusline still shows a small
  `⬆ mewxi update` notice when a newer version is available.

Other knobs in `accounts.toml`: `update_prompt = false` silences the
startup question (the statusline notice stays); `update_check = false`
turns off automatic checks entirely — no fetch on TUI startup, none
from the watcher (`mewxi update`, `--check`, and the Config view's
"check for updates" row still work). Both are also toggleable from the
Config view.

## Optional: watcher service

```bash
mewxi setup --service
```

Installs a user-scope `systemd` unit (Linux) or `launchd` agent (macOS)
that runs `mewxi watch` at login. Without it, the statusLine still works —
it just recomputes on every Claude Code tick instead of reading a cache.

Uninstall with `mewxi stop --disable`.
