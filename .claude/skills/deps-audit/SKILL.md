---
name: deps-audit
description: Audit outdated cargo dependencies — diff each update's source between the current and new version, review the diff for backdoors/supply-chain red flags, then update the safe ones. Use when the pre-commit/pre-push deps-check hook reports outdated dependencies, or when the user asks to update or audit cargo deps.
---

# Cargo dependency audit & update

Goal: for every outdated dependency, review the *actual source diff* between
the pinned version and the candidate version before updating. Never update a
dependency whose diff you have not reviewed.

## 1. Enumerate outdated deps

- Prefer `cargo outdated --root-deps-only` (install with
  `cargo install cargo-outdated` if missing — ask first, it takes a while).
  Fallback: `cargo update --dry-run` (compatible bumps only).
- Also run `cargo audit` if available to pick up known RustSec advisories for
  both current and candidate versions.
- Build a table: name, current version, candidate version, semver-compatible
  or major bump.

## 2. Fetch and diff each update's source

For each dep, work in the scratchpad directory:

```
curl -sL https://static.crates.io/crates/<name>/<name>-<old>.crate | tar xz
curl -sL https://static.crates.io/crates/<name>/<name>-<new>.crate | tar xz
diff -ru <name>-<old> <name>-<new> > <name>.diff
```

The `.crate` file is what cargo actually builds — audit it, not the GitHub
repo (they can differ; a mismatch is itself a red flag worth checking for
suspicious files like an unexpected `build.rs`).

For transitive updates surfaced by `cargo update --dry-run`, audit at least
those that have a `build.rs`, are proc-macros, or jump multiple versions.

## 3. Review each diff for supply-chain red flags

Read the full diff for small deps; for large diffs, prioritize:

- **`build.rs` and proc-macro code** — anything new/changed here runs on the
  build machine. Scrutinize every change.
- **New capabilities the crate didn't need before**: process spawning
  (`Command`, `exec`), network (`TcpStream`, http clients, DNS), filesystem
  access outside its purpose, reading env vars (`CARGO_*`, `HOME`, tokens,
  `SSH_*`), `unsafe` blocks appearing in safe-looking code.
- **Obfuscation**: base64/hex blobs, `include_bytes!` of opaque files,
  string concatenation that assembles identifiers/URLs at runtime, zip/xz
  payloads in the tarball, minified or machine-generated-looking code in a
  hand-written crate.
- **Manifest changes**: new dependencies (typosquats — check spelling against
  crates.io), new features enabled by default, changed `links`/`build` keys.
- **Release hygiene**: does the diff match the crate's changelog/release
  notes? A published version with no corresponding git tag, or a diff much
  larger than the changelog implies, warrants a closer look. Check for a
  recent maintainer/owner change on crates.io if anything feels off.
- **Yanked-adjacent signals**: candidate version published very recently
  (hours/days) — consider waiting; check the RustSec advisory DB and the
  crate's issue tracker for reports.

Verdict per dep: **clean**, **suspicious (explain exactly what and where)**,
or **too large to fully review** (say what was and wasn't covered — no
silent partial reviews).

## 4. Update

Only for deps verdicted clean:

- Compatible bumps: `cargo update -p <name>`.
- Major bumps: edit `Cargo.toml`, then fix any breakage; read the release
  notes for migration steps.
- Never update a suspicious dep; report it and pin the current version if
  the concern is in a transitive update (`cargo update -p <name> --precise <ver>`).

## 5. Verify

- `cargo build` and `cargo test` must pass.
- `cargo tree -d` to check the update didn't introduce duplicate major
  versions.

## 6. Report

Summarize: what was updated, what was held back and why, per-dep verdict
with the specific evidence reviewed (diff size, files read). Commit as
`chore(deps): ...` only if the user asked for a commit.
