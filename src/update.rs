//! Self-update: check the source checkout's git remote for a newer
//! mewxi and rebuild via `cargo install`.
//!
//! mewxi is installed from a git checkout (`cargo install --path .`),
//! so "update" means: fetch the remote, compare, clone the target ref
//! into a throwaway build dir (the OS temp dir by default,
//! `update_build_dir` in `accounts.toml` to override) and re-run the
//! install from there — the source checkout itself is never touched.
//! Two channels, picked in
//! `accounts.toml` (`update_channel`) or from the TUI's Config view:
//!
//! - `release` — follow version tags (`v0.2.0`). An update exists when
//!   the highest tag on origin is newer than the running
//!   `CARGO_PKG_VERSION`.
//! - `dev` — follow origin's default branch (main). An update exists
//!   when that branch has commits the *running binary* wasn't built
//!   from (the build commit is embedded by `build.rs`; comparing the
//!   checkout's HEAD instead would miss updates whenever you develop
//!   in the same checkout the binary was installed from).
//!
//! The source checkout is located via the compile-time
//! `MEWXI_SOURCE_REPO` embedded by `build.rs` (the dir that was built,
//! or — for binaries the self-updater built in a temp clone — the
//! original checkout that clone came from), overridable with
//! `update_repo_dir` in `accounts.toml` when the checkout has moved
//! since the build.
//!
//! Binaries downloaded prebuilt (GitHub Actions artifacts, Releases)
//! have no source checkout on the machine at all — `MEWXI_SOURCE_REPO`
//! there is the *builder's* path and never resolves locally. For that
//! case `build.rs` also embeds `MEWXI_ORIGIN_URL` (the repo's `origin`
//! remote at build time). When the local checkout can't be found but
//! that URL is non-empty, checks and applies fall back to a
//! remote-only path driven by `git ls-remote <url>` — no local repo
//! needed, git still required on PATH. It's strictly less capable than
//! the local path (no behind-count for the dev channel, since that
//! needs a merge-base walk against a local ref), but it's the
//! difference between the updater working at all and silently dying
//! for anyone not running from a checkout. See [`RepoSource`].
//!
//! Every successful check is cached at
//! `~/.cache/mewxi/update-check.json` so cheap consumers — most
//! importantly the statusLine renderer that runs inside every Claude
//! Code session — can show an "update available" notice without
//! touching the network. The TUI checks on every startup (kicked the
//! moment the splash appears, with a fresh cache pre-seeding the
//! verdict while the check runs); the `watch` daemon refreshes the
//! cache every few hours, skipping while it's still fresh. Both
//! automatic checks honor `update_check = false` in `accounts.toml`;
//! explicit checks (`mewxi update`, the Config view row) always run.

use crate::accounts::{self, AccountsView};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UpdateChannel {
    /// Follow version tags — stable releases only.
    Release,
    /// Follow origin's default branch — every pushed commit.
    Dev,
}

impl UpdateChannel {
    /// Parse the `update_channel` config value. Anything that isn't an
    /// explicit dev spelling falls back to Release (the safe default).
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("dev") | Some("main") | Some("development") => UpdateChannel::Dev,
            _ => UpdateChannel::Release,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UpdateChannel::Release => "release",
            UpdateChannel::Dev => "dev",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UpdateChannel::Release => "release — tagged versions",
            UpdateChannel::Dev => "dev — follow main branch",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            UpdateChannel::Release => UpdateChannel::Dev,
            UpdateChannel::Dev => UpdateChannel::Release,
        }
    }
}

pub fn channel_from_view(view: &AccountsView) -> UpdateChannel {
    UpdateChannel::from_config(view.update_channel.as_deref())
}

// ---------------------------------------------------------------------------
// Check interval
// ---------------------------------------------------------------------------

/// How long a cached check stays fresh before automatic checks (TUI
/// startup, watch daemon) hit origin again.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UpdateInterval {
    Min15,
    Hour1,
    Hour6,
    Hour24,
}

impl UpdateInterval {
    /// Parse the `update_interval` config value. Anything unrecognized
    /// falls back to 6h (the historical hardcoded cadence).
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("15m") => UpdateInterval::Min15,
            Some("1h") => UpdateInterval::Hour1,
            Some("24h") => UpdateInterval::Hour24,
            _ => UpdateInterval::Hour6,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UpdateInterval::Min15 => "15m",
            UpdateInterval::Hour1 => "1h",
            UpdateInterval::Hour6 => "6h",
            UpdateInterval::Hour24 => "24h",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UpdateInterval::Min15 => "every 15 minutes",
            UpdateInterval::Hour1 => "every hour",
            UpdateInterval::Hour6 => "every 6 hours",
            UpdateInterval::Hour24 => "once a day",
        }
    }

    /// Next option in the Config view's Enter-to-cycle order.
    pub fn cycled(self) -> Self {
        match self {
            UpdateInterval::Min15 => UpdateInterval::Hour1,
            UpdateInterval::Hour1 => UpdateInterval::Hour6,
            UpdateInterval::Hour6 => UpdateInterval::Hour24,
            UpdateInterval::Hour24 => UpdateInterval::Min15,
        }
    }

    pub fn max_age(self) -> chrono::Duration {
        match self {
            UpdateInterval::Min15 => chrono::Duration::minutes(15),
            UpdateInterval::Hour1 => chrono::Duration::hours(1),
            UpdateInterval::Hour6 => chrono::Duration::hours(6),
            UpdateInterval::Hour24 => chrono::Duration::hours(24),
        }
    }
}

pub fn interval_from_view(view: &AccountsView) -> UpdateInterval {
    UpdateInterval::from_config(view.update_interval.as_deref())
}

// ---------------------------------------------------------------------------
// Check result + on-disk cache
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct UpdateStatus {
    pub channel: UpdateChannel,
    pub available: bool,
    /// What's running now: a version for the release channel, a short
    /// commit hash for dev.
    pub current: String,
    /// What origin offers: the newest tag, or origin/main's short hash.
    pub latest: String,
    /// One-line human explanation ("3 commit(s) behind origin/main").
    pub detail: String,
}

/// JSON shape persisted at [`cache_path`]. Mirrors [`UpdateStatus`]
/// plus a timestamp so consumers can ignore stale results.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedCheck {
    pub checked_at: DateTime<Utc>,
    pub channel: String,
    pub available: bool,
    pub current: String,
    pub latest: String,
    pub detail: String,
}

pub fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("mewxi").join("update-check.json"))
}

pub fn load_cached() -> Option<CachedCheck> {
    let path = cache_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::FileRead,
                &format!("{} unreadable — {}", basename(&path), first_line(&e.to_string())),
            );
            return None;
        }
    };
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Update,
        crate::debug_log::LogKind::FileRead,
        &format!("read {}", basename(&path)),
    );
    serde_json::from_str(&raw).ok()
}

fn write_cache(status: &UpdateStatus) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cached = CachedCheck {
        checked_at: Utc::now(),
        channel: status.channel.as_str().to_string(),
        available: status.available,
        current: status.current.clone(),
        latest: status.latest.clone(),
        detail: status.detail.clone(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&cached) {
        let tmp = path.with_extension("json.tmp");
        match std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path)) {
            Ok(()) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::FileWrite,
                &format!("wrote {}", basename(&path)),
            ),
            Err(e) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::FileWrite,
                &format!("{} write failed — {}", basename(&path), first_line(&e.to_string())),
            ),
        }
    }
}

/// True when the cached check is recent enough that re-fetching origin
/// would be wasted work.
fn cache_is_fresh(interval: UpdateInterval) -> bool {
    load_cached().is_some_and(|c| Utc::now() - c.checked_at < interval.max_age())
}

/// The cached check as an [`UpdateStatus`], but only when it's fresher
/// than `interval` and was taken against `channel` — a stale or
/// cross-channel cache is no substitute for a real check.
pub fn fresh_cached_status(
    channel: UpdateChannel,
    interval: UpdateInterval,
) -> Option<UpdateStatus> {
    let c = load_cached()?;
    if Utc::now() - c.checked_at >= interval.max_age() {
        return None;
    }
    if UpdateChannel::from_config(Some(&c.channel)) != channel {
        return None;
    }
    Some(UpdateStatus {
        channel,
        available: c.available,
        current: c.current,
        latest: c.latest,
        detail: c.detail,
    })
}

// ---------------------------------------------------------------------------
// Repo discovery + git plumbing
// ---------------------------------------------------------------------------

/// Locate the source checkout this binary was built from. The
/// compile-time `MEWXI_SOURCE_REPO` (embedded by `build.rs`: the dir
/// that was built, or the original checkout when the self-updater
/// built in a temp clone) is authoritative; `update_repo_dir` in
/// `accounts.toml` overrides it when the checkout has moved since the
/// build.
pub fn repo_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    let dir = override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("MEWXI_SOURCE_REPO")));
    if !dir.is_dir() {
        return Err(anyhow!(
            "source checkout not found at {} — set update_repo_dir in accounts.toml",
            dir.display()
        ));
    }
    git(&dir, &["rev-parse", "--git-dir"]).map_err(|_| {
        anyhow!(
            "{} is not a git checkout — set update_repo_dir in accounts.toml",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// Last path component, for log messages — full paths are noise in the
/// panel and mostly redundant with the surrounding message.
fn basename(p: &Path) -> std::borrow::Cow<'_, str> {
    p.file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| p.to_string_lossy())
}

/// First line of a possibly multi-line OS/stderr message — the rest is
/// rarely useful in a one-line log entry.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Format a duration the way the log panel expects: sub-second in ms,
/// otherwise seconds with one decimal (e.g. `142ms`, `1.2s`).
fn fmt_dur(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Build a short `git <subcommand> [<arg>]` label for log messages —
/// the full arg list is noise, and a bare remote URL can embed
/// credentials, so only the subcommand plus the first identifying,
/// non-URL argument is kept.
fn git_label(args: &[&str]) -> String {
    let cmd = args.first().copied().unwrap_or("git");
    let target = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && !a.contains("://"));
    match target {
        Some(t) => format!("git {cmd} {t}"),
        None => format!("git {cmd}"),
    }
}

/// Run `git -C <repo> <args>` and return trimmed stdout. Errors carry
/// stderr so failures (auth, network) read meaningfully in the TUI.
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let start = std::time::Instant::now();
    let result = Command::new("git").arg("-C").arg(repo).args(args).output();
    let dur_ms = start.elapsed().as_millis();
    let label = git_label(args);
    let out = match result {
        Ok(o) => o,
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("{label} failed — {}", first_line(&e.to_string())),
            );
            return Err(e).with_context(|| format!("running git {}", args.join(" ")));
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = first_line(&stderr);
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::Proc,
            &format!("{label} failed — {reason}"),
        );
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Update,
        crate::debug_log::LogKind::Proc,
        &format!("{label} · {}", fmt_dur(dur_ms)),
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a bare `git <args>` with no `-C <repo>` — for `ls-remote`
/// against a URL, which needs no local checkout at all. Same
/// error-with-stderr shape as [`git`].
fn git_norepo(args: &[&str]) -> Result<String> {
    let start = std::time::Instant::now();
    let result = Command::new("git").args(args).output();
    let dur_ms = start.elapsed().as_millis();
    let label = git_label(args);
    let out = match result {
        Ok(o) => o,
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("{label} failed — {}", first_line(&e.to_string())),
            );
            return Err(e).with_context(|| format!("running git {}", args.join(" ")));
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = first_line(&stderr);
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::Proc,
            &format!("{label} failed — {reason}"),
        );
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Update,
        crate::debug_log::LogKind::Proc,
        &format!("{label} · {}", fmt_dur(dur_ms)),
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit this binary was built from, embedded by `build.rs`.
/// Empty when the build had no git context (e.g. a source tarball).
const BUILD_COMMIT: &str = env!("MEWXI_BUILD_COMMIT");

/// The repo's `origin` URL at build time, embedded by `build.rs`.
/// Empty when the build had no git context. This is what lets
/// prebuilt-binary users (no local checkout, so [`repo_dir`] fails)
/// still check for and apply updates — see [`RepoSource`].
const ORIGIN_URL: &str = env!("MEWXI_ORIGIN_URL");

/// Where to run update checks/applies against: a local checkout (the
/// normal case — richer checks, e.g. dev-channel behind-counts) or,
/// when no local checkout can be found but the binary was built with a
/// known `origin`, the bare URL via `git ls-remote`/`git clone`.
enum RepoSource {
    Local(PathBuf),
    Remote(String),
}

/// Resolve the checkout to operate against: [`repo_dir`] if it
/// resolves, else a remote fallback using the build-embedded
/// [`ORIGIN_URL`] when one exists. Only errors when both are
/// unavailable — in which case the [`repo_dir`] error (which already
/// names `update_repo_dir` as the fix) is the right message to surface.
fn resolve_repo_source(override_dir: Option<&Path>) -> Result<RepoSource> {
    match repo_dir(override_dir) {
        Ok(dir) => Ok(RepoSource::Local(dir)),
        Err(e) => {
            if ORIGIN_URL.is_empty() {
                Err(e)
            } else {
                Ok(RepoSource::Remote(ORIGIN_URL.to_string()))
            }
        }
    }
}

/// Baseline for the dev-channel comparison: the commit the running
/// binary was built from, when it's known and still exists in `repo`;
/// otherwise the checkout's HEAD. The build commit is what makes the
/// notice fire after committing+pushing from the same checkout the
/// binary was installed from — HEAD is already in sync with origin at
/// that point, but the binary isn't. The HEAD fallback covers builds
/// without git context and repos whose history was rewritten since.
fn dev_baseline(repo: &Path, build_commit: &str) -> Result<String> {
    if !build_commit.is_empty() {
        let verify = format!("{build_commit}^{{commit}}");
        if git(repo, &["rev-parse", "--verify", "--quiet", &verify]).is_ok() {
            return Ok(build_commit.to_string());
        }
    }
    git(repo, &["rev-parse", "--short", "HEAD"])
}

/// Origin's default branch: `origin/HEAD` when the clone recorded it,
/// otherwise the first of main/master that exists remotely.
fn remote_default_branch(repo: &Path) -> String {
    if let Ok(s) = git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(b) = s.strip_prefix("origin/") {
            return b.to_string();
        }
    }
    for b in ["main", "master"] {
        if git(repo, &["rev-parse", "--verify", "--quiet", &format!("origin/{b}")]).is_ok() {
            return b.to_string();
        }
    }
    "main".to_string()
}

/// Newest version-looking tag (sorted by version, descending). Tags
/// that don't start with a digit after an optional `v` are skipped.
fn latest_version_tag(repo: &Path) -> Result<Option<String>> {
    let out = git(repo, &["tag", "--list", "--sort=-v:refname"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .find(|t| {
            t.trim_start_matches('v')
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
        .map(String::from))
}

/// Parse `git ls-remote --tags --refs <url>` output (`<sha>\trefs/tags/<name>`
/// per line) into version-looking tag names — same digit-after-optional-`v`
/// filter as [`latest_version_tag`]. Pure string parsing, no network, so
/// it's covered by a unit test below.
fn parse_ls_remote_tags(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .filter_map(|refname| refname.strip_prefix("refs/tags/"))
        .filter(|t| {
            t.trim_start_matches('v')
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
        .map(String::from)
        .collect()
}

/// Parse `git ls-remote --symref <url> HEAD` output for the default
/// branch: the `ref: refs/heads/<branch>\tHEAD` line git prints ahead
/// of the sha line when `--symref` is given.
fn parse_ls_remote_symref_head(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.strip_prefix("ref: refs/heads/")?;
        rest.split_whitespace().next().map(String::from)
    })
}

/// Loose semver: optional `v` prefix, numeric dot components, anything
/// after `-`/`+` ignored. Compares as a numeric vector, so `v0.10.0`
/// beats `v0.9.1`.
fn parse_version(s: &str) -> Vec<u64> {
    s.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

fn version_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

/// Fetch origin and compare per the configured channel. Network +
/// subprocess heavy — call from a background thread in interactive
/// contexts. Writes the cache on success. Uses the local checkout when
/// [`resolve_repo_source`] finds one, else falls back to a remote-only
/// check against the build-embedded origin URL.
pub fn check_now() -> Result<UpdateStatus> {
    match check_now_inner() {
        Ok(status) => {
            let msg = if status.available {
                format!(
                    "update available — {} ({})",
                    status.latest,
                    status.channel.as_str()
                )
            } else {
                format!(
                    "up to date — {} ({})",
                    status.current,
                    status.channel.as_str()
                )
            };
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Info,
                &msg,
            );
            Ok(status)
        }
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("check failed — {}", first_line(&e.to_string())),
            );
            Err(e)
        }
    }
}

fn check_now_inner() -> Result<UpdateStatus> {
    let view = accounts::load_accounts()?;
    let channel = channel_from_view(&view);
    let status = match resolve_repo_source(view.update_repo_dir.as_deref())? {
        RepoSource::Local(repo) => check_local(&repo, channel)?,
        RepoSource::Remote(url) => check_remote(&url, channel)?,
    };
    write_cache(&status);
    Ok(status)
}

/// [`check_now`]'s local-checkout path: fetch, then compare against
/// origin's tags (release) or default branch (dev).
fn check_local(repo: &Path, channel: UpdateChannel) -> Result<UpdateStatus> {
    git(repo, &["fetch", "--quiet", "--tags", "origin"]).context("fetching origin")?;

    Ok(match channel {
        UpdateChannel::Release => {
            let current = env!("CARGO_PKG_VERSION").to_string();
            match latest_version_tag(repo)? {
                Some(tag) => {
                    let available = version_newer(&tag, &current);
                    let detail = if available {
                        format!("tag {tag} is newer than v{current}")
                    } else {
                        format!("v{current} matches the newest tag")
                    };
                    UpdateStatus { channel, available, current: format!("v{current}"), latest: tag, detail }
                }
                None => UpdateStatus {
                    channel,
                    available: false,
                    current: format!("v{current}"),
                    latest: format!("v{current}"),
                    detail: "no version tags on origin yet".to_string(),
                },
            }
        }
        UpdateChannel::Dev => {
            let branch = remote_default_branch(repo);
            let current = dev_baseline(repo, BUILD_COMMIT)?;
            let behind: u64 = git(repo, &["rev-list", "--count", &format!("{current}..origin/{branch}")])?
                .parse()
                .unwrap_or(0);
            let latest = git(repo, &["rev-parse", "--short", &format!("origin/{branch}")])?;
            UpdateStatus {
                channel,
                available: behind > 0,
                current,
                latest,
                detail: if behind > 0 {
                    format!("{behind} commit(s) behind origin/{branch}")
                } else {
                    format!("in sync with origin/{branch}")
                },
            }
        }
    })
}

/// [`check_now`]'s remote-only fallback for prebuilt binaries with no
/// local checkout: `git ls-remote` straight against `url`, no fetch or
/// local repo needed (git on PATH still required). Can't produce a
/// behind-count for the dev channel — that needs a merge-base walk
/// against a local ref — so it reports a plain sha comparison instead.
fn check_remote(url: &str, channel: UpdateChannel) -> Result<UpdateStatus> {
    Ok(match channel {
        UpdateChannel::Release => {
            let current = env!("CARGO_PKG_VERSION").to_string();
            let out = git_norepo(&["ls-remote", "--tags", "--refs", url])
                .context("ls-remote --tags origin")?;
            match parse_ls_remote_tags(&out).into_iter().max_by_key(|t| parse_version(t)) {
                Some(tag) => {
                    let available = version_newer(&tag, &current);
                    let detail = if available {
                        format!("tag {tag} is newer than v{current}")
                    } else {
                        format!("v{current} matches the newest tag")
                    };
                    UpdateStatus { channel, available, current: format!("v{current}"), latest: tag, detail }
                }
                None => UpdateStatus {
                    channel,
                    available: false,
                    current: format!("v{current}"),
                    latest: format!("v{current}"),
                    detail: "no version tags on origin yet".to_string(),
                },
            }
        }
        UpdateChannel::Dev => {
            if BUILD_COMMIT.is_empty() {
                return Ok(UpdateStatus {
                    channel,
                    available: false,
                    current: String::new(),
                    latest: String::new(),
                    detail: "binary has no build-commit metadata to compare against origin"
                        .to_string(),
                });
            }
            let out = git_norepo(&["ls-remote", url, "HEAD"]).context("ls-remote origin HEAD")?;
            let remote_sha = out.split_whitespace().next().unwrap_or("");
            if remote_sha.is_empty() {
                return Err(anyhow!("git ls-remote {url} HEAD returned no commit"));
            }
            // Match BUILD_COMMIT's own abbreviation length so the two
            // read as the same kind of thing side by side.
            let short_len = BUILD_COMMIT.len().max(7).min(remote_sha.len());
            let short = &remote_sha[..short_len];
            let in_sync = remote_sha.starts_with(BUILD_COMMIT);
            UpdateStatus {
                channel,
                available: !in_sync,
                current: BUILD_COMMIT.to_string(),
                latest: short.to_string(),
                detail: if in_sync {
                    "in sync with origin HEAD".to_string()
                } else {
                    format!("origin HEAD {short} differs from build {BUILD_COMMIT}")
                },
            }
        }
    })
}

/// Run [`check_now`] on a background thread, delivering the result
/// over `tx`. Errors are stringified so the receiver stays Send-simple.
pub fn spawn_check(tx: std::sync::mpsc::Sender<std::result::Result<UpdateStatus, String>>) {
    std::thread::spawn(move || {
        let res = check_now().map_err(|e| e.to_string());
        let _ = tx.send(res);
    });
}

/// Fire-and-forget cache refresh, skipped when the cache is still
/// fresher than the configured `update_interval` or when
/// `update_check = false` turned automatic checks off. Used by the
/// `watch` daemon so the statusLine notice stays honest without the
/// TUI ever running.
pub fn refresh_cache_async() {
    let view = accounts::load_accounts().ok();
    let auto_enabled = view.as_ref().map(|v| v.update_check).unwrap_or(true);
    let interval = view
        .as_ref()
        .map(interval_from_view)
        .unwrap_or(UpdateInterval::Hour6);
    if !auto_enabled || cache_is_fresh(interval) {
        return;
    }
    std::thread::spawn(|| {
        let _ = check_now();
    });
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// Where update builds happen: a per-run folder under the configured
/// `update_build_dir`, or the OS temp dir (`/tmp` on Unix, `%TEMP%` on
/// Windows) by default. Per-process name so concurrent updates can't
/// trample each other.
fn build_workdir(override_dir: Option<&Path>) -> PathBuf {
    let root = override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    root.join(format!("mewxi-update-{}", std::process::id()))
}

/// True when `cargo` resolves on PATH. Prebuilt-binary users may have
/// only the mewxi binary and no Rust toolchain — self-update builds
/// from source, so we want a clear message instead of a raw "No such
/// file or directory" from the failed `Command::new("cargo")` spawn.
fn cargo_available() -> bool {
    let start = std::time::Instant::now();
    let ok = Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let msg = if ok {
        format!("cargo --version · {}", fmt_dur(start.elapsed().as_millis()))
    } else {
        "cargo --version failed — not on PATH".to_string()
    };
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Update,
        crate::debug_log::LogKind::Proc,
        &msg,
    );
    ok
}

/// Build the channel's newest point in a throwaway clone and install
/// it via `cargo install --path <clone> --force` — the source checkout
/// (when there is one) is only consulted for its origin URL and never
/// modified, so a dirty working tree or a checked-out feature branch is
/// fine. Falls back to a remote-only resolution (no local checkout
/// needed) when [`resolve_repo_source`] can't find a local repo but the
/// binary was built with a known origin URL — see the module docs.
/// Streams git/cargo output to the inherited stdout/stderr — call only
/// with the terminal restored (the TUI suspends its alternate screen
/// around this).
pub fn apply_now() -> Result<String> {
    let view = accounts::load_accounts()?;
    let channel = channel_from_view(&view);

    println!("mewxi update ({})", channel.as_str());

    if !cargo_available() {
        return Err(anyhow!(
            "cargo not found — self-update builds from source; install Rust (rustup.rs) or download a prebuilt release"
        ));
    }

    // `source_repo` is Some only for the local path — it's forwarded to
    // the rebuild below as MEWXI_SOURCE_REPO so the new binary remembers
    // the real checkout instead of baking in the throwaway clone dir.
    // In remote mode there's no local checkout to remember: the clone's
    // own `origin` remote (the real URL) is genuine, so leaving
    // MEWXI_SOURCE_REPO unset lets the new binary's build.rs pick up
    // both MEWXI_SOURCE_REPO (harmlessly wrong — it'll fail like this
    // run did) and MEWXI_ORIGIN_URL (correctly, from the clone's own git
    // context) — the new binary self-heals into the same remote-only
    // path rather than losing update capability entirely.
    let (origin, git_ref, source_repo) = match resolve_repo_source(view.update_repo_dir.as_deref())? {
        RepoSource::Local(repo) => {
            println!("  fetching origin …");
            git(&repo, &["fetch", "--quiet", "--tags", "origin"]).context("fetching origin")?;
            let origin = git(&repo, &["remote", "get-url", "origin"]).context("reading origin URL")?;
            let git_ref = match channel {
                UpdateChannel::Release => latest_version_tag(&repo)?.ok_or_else(|| {
                    anyhow!("no version tags on origin — switch to the dev channel?")
                })?,
                UpdateChannel::Dev => remote_default_branch(&repo),
            };
            (origin, git_ref, Some(repo))
        }
        RepoSource::Remote(url) => {
            println!("  resolving {url} …");
            let git_ref = match channel {
                UpdateChannel::Release => {
                    let out = git_norepo(&["ls-remote", "--tags", "--refs", &url])
                        .context("ls-remote --tags origin")?;
                    parse_ls_remote_tags(&out)
                        .into_iter()
                        .max_by_key(|t| parse_version(t))
                        .ok_or_else(|| {
                            anyhow!("no version tags on origin — switch to the dev channel?")
                        })?
                }
                UpdateChannel::Dev => {
                    let out = git_norepo(&["ls-remote", "--symref", &url, "HEAD"])
                        .context("ls-remote --symref origin HEAD")?;
                    parse_ls_remote_symref_head(&out).unwrap_or_else(|| "main".to_string())
                }
            };
            (url, git_ref, None)
        }
    };

    let workdir = build_workdir(view.update_build_dir.as_deref());
    if workdir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&workdir) {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("build workdir clear failed — {}", first_line(&e.to_string())),
            );
            return Err(e).with_context(|| format!("clearing stale build dir {}", workdir.display()));
        }
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::FileWrite,
            "cleared build workdir",
        );
    }
    if let Some(parent) = workdir.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("build workdir create failed — {}", first_line(&e.to_string())),
            );
            return Err(e).with_context(|| format!("creating build dir {}", parent.display()));
        }
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::FileWrite,
            "created build workdir",
        );
    }
    println!("  cloning {git_ref} into {} …", workdir.display());
    let clone_start = std::time::Instant::now();
    let clone_result = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", "--branch", &git_ref])
        .arg(&origin)
        .arg(&workdir)
        .status();
    let clone_dur_ms = clone_start.elapsed().as_millis();
    let status = match clone_result {
        Ok(s) => s,
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("git clone failed — {}", first_line(&e.to_string())),
            );
            return Err(e).context("running git clone");
        }
    };
    if status.success() {
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::Proc,
            &format!("git clone · {}", fmt_dur(clone_dur_ms)),
        );
    } else {
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::Proc,
            &format!("git clone failed — exit {status}"),
        );
    }
    if !status.success() {
        return Err(anyhow!("git clone failed ({status})"));
    }
    let target = match channel {
        UpdateChannel::Release => git_ref.clone(),
        UpdateChannel::Dev => git(&workdir, &["rev-parse", "--short", "HEAD"])?,
    };

    println!("  building (cargo install --path … --force) — this can take a minute …");
    // MEWXI_SOURCE_REPO: make the new binary remember the real source
    // checkout, not this throwaway clone (build.rs embeds it) — else
    // its own update checks point at a dir we delete a few lines down.
    // Left unset in remote mode (source_repo is None) — see the comment
    // above on why that's the right call there.
    let mut cargo_cmd = Command::new("cargo");
    cargo_cmd.arg("install").arg("--path").arg(&workdir).arg("--force");
    if let Some(repo) = &source_repo {
        cargo_cmd.env("MEWXI_SOURCE_REPO", repo);
    }
    let build_start = std::time::Instant::now();
    let build_result = cargo_cmd.status();
    let build_dur_ms = build_start.elapsed().as_millis();
    let status = match build_result {
        Ok(s) => s,
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Update,
                crate::debug_log::LogKind::Error,
                &format!("cargo install failed — {}", first_line(&e.to_string())),
            );
            return Err(e).context("running cargo install (is cargo on PATH?)");
        }
    };
    if status.success() {
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::Proc,
            &format!("cargo install · {}", fmt_dur(build_dur_ms)),
        );
    } else {
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::Proc,
            &format!("cargo install failed — exit {status}"),
        );
    }
    // Best-effort cleanup either way — the clone is throwaway.
    let cleanup_ok = std::fs::remove_dir_all(&workdir).is_ok();
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Update,
        crate::debug_log::LogKind::FileWrite,
        if cleanup_ok {
            "removed build workdir"
        } else {
            "build workdir cleanup failed"
        },
    );
    if !status.success() {
        return Err(anyhow!("cargo install failed ({status})"));
    }

    // `cargo install` always lands in cargo's bin dir — but the mewxi
    // the user actually runs may live somewhere else entirely (a copy
    // in ~/.local/bin that shadows ~/.cargo/bin on PATH, /usr/local/bin,
    // …). If we stop here, the running binary never changes, the next
    // check still sees the old version, and the update looks like it
    // never happened. So: overwrite the running executable's real path
    // with the freshly-installed binary too. Skipped for dev builds
    // running out of the repo's target/ dir — those belong to cargo.
    let mut synced_note = String::new();
    let installed = cargo_bin_path();
    if let Ok(running) = std::env::current_exe() {
        let running = std::fs::canonicalize(&running).unwrap_or(running);
        let installed_real = std::fs::canonicalize(&installed).unwrap_or_else(|_| installed.clone());
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Update,
            crate::debug_log::LogKind::FileRead,
            if running == installed_real {
                "binary check · in sync"
            } else {
                "binary check · differs"
            },
        );
        let is_dev_build = source_repo
            .as_ref()
            .is_some_and(|repo| running.starts_with(repo.join("target")));
        if running != installed_real && !is_dev_build && installed_real.is_file() {
            replace_binary(&installed_real, &running).with_context(|| {
                format!(
                    "installing over the running binary at {}",
                    running.display()
                )
            })?;
            println!("  synced {} → {}", installed.display(), running.display());
            synced_note = format!(" (installed to {})", running.display());
        }
    }

    // The freshly-built binary IS the latest now — flip the cache so
    // the statusLine notice clears immediately.
    write_cache(&UpdateStatus {
        channel,
        available: false,
        current: target.clone(),
        latest: target.clone(),
        detail: "just updated".to_string(),
    });

    Ok(format!(
        "mewxi updated to {target}{synced_note} — restart mewxi to load it"
    ))
}

/// Replace the current process with the freshly-installed binary,
/// re-running the same command line. Both `cargo install` and
/// [`replace_binary`] swap the file in via rename, so the path this
/// process was started from now holds the new build — exec'ing it *is*
/// the restart. Call only with the terminal fully restored (the new
/// process sets up its own alternate screen). Returns the error when
/// the exec itself fails; on success it never returns.
pub fn restart_process() -> std::io::Error {
    let exe = std::env::current_exe()
        .ok()
        .filter(|p| p.is_file())
        .unwrap_or_else(cargo_bin_path);
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Update,
        crate::debug_log::LogKind::Info,
        "restarting into new binary",
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` replaces this image — on success it never returns.
        Command::new(exe).args(std::env::args_os().skip(1)).exec()
    }
    #[cfg(windows)]
    {
        // Windows has no `exec`: spawn the freshly-installed binary as a
        // child wired to the same stdio, wait for it, then exit with its
        // code so the parent terminal hands control to the new process.
        match Command::new(exe).args(std::env::args_os().skip(1)).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(0)),
            Err(e) => e,
        }
    }
}

/// Where `cargo install` puts binaries: `$CARGO_INSTALL_ROOT`, then
/// `$CARGO_HOME`, then `~/.cargo` — each with `/bin/mewxi` appended.
fn cargo_bin_path() -> PathBuf {
    let root = std::env::var_os("CARGO_INSTALL_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .unwrap_or_default();
    root.join("bin").join(MEWXI_BIN_NAME)
}

/// Executable file name `cargo install` produces for this crate —
/// `mewxi.exe` on Windows, `mewxi` elsewhere.
#[cfg(windows)]
const MEWXI_BIN_NAME: &str = "mewxi.exe";
#[cfg(not(windows))]
const MEWXI_BIN_NAME: &str = "mewxi";

/// Atomically replace `dst` with a copy of `src` (copy to a sibling
/// tempfile, then rename). The running process keeps its old inode, so
/// overwriting a live executable's path is safe on unix.
///
/// On Windows a file that is currently executing can't be renamed *over*
/// (sharing violation), but it *can* be renamed *away*. So when the
/// straight rename fails we move the live `dst` aside to a `.old-…`
/// sidecar first, then drop the new binary in. The stale sidecar is
/// best-effort deleted (it's still mapped until the old process exits;
/// a later update sweeps it).
fn replace_binary(src: &Path, dst: &Path) -> Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".update-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::copy(src, &tmp)
        .with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, dst) {
        #[cfg(windows)]
        {
            // dst is likely the running exe — move it out of the way and retry.
            let mut aside = dst.as_os_str().to_owned();
            aside.push(".old-update");
            let aside = PathBuf::from(aside);
            let _ = std::fs::remove_file(&aside);
            if std::fs::rename(dst, &aside).is_ok() {
                if let Err(e2) = std::fs::rename(&tmp, dst) {
                    // Roll back so we don't leave dst missing.
                    let _ = std::fs::rename(&aside, dst);
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e2)
                        .with_context(|| format!("renaming into {}", dst.display()));
                }
                let _ = std::fs::remove_file(&aside);
                return Ok(());
            }
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("renaming into {}", dst.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// statusLine notice
// ---------------------------------------------------------------------------

/// Small ANSI segment appended to the Claude Code statusLine when the
/// cached check says an update exists. `None` when up to date, never
/// checked, or the cache is old enough to distrust (14 days).
pub fn statusline_segment() -> Option<String> {
    let c = load_cached()?;
    if !c.available {
        return None;
    }
    if Utc::now() - c.checked_at > chrono::Duration::days(14) {
        return None;
    }
    let what = if c.channel == "dev" {
        format!("update ({})", c.latest)
    } else {
        format!("update {}", c.latest)
    };
    // Leading-style segment (trailing separator) so it can sit at the
    // front of the statusline and stay visible when narrow terminals
    // truncate the tail. Mirrors the setup-incomplete hint's shape.
    Some(format!(
        "\x1b[35m↑ mewxi {what}\x1b[0m \x1b[90m|\x1b[0m "
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_handles_prefix_and_suffix() {
        assert_eq!(parse_version("v1.2.3"), vec![1, 2, 3]);
        assert_eq!(parse_version("1.2.3-rc.1"), vec![1, 2, 3]);
        assert_eq!(parse_version("0.10"), vec![0, 10]);
    }

    #[test]
    fn version_newer_is_numeric_not_lexical() {
        assert!(version_newer("v0.10.0", "0.9.1"));
        assert!(version_newer("1.0.0", "0.99.99"));
        assert!(!version_newer("v0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "v0.2.0"));
        // Longer wins on equal prefix: 0.1.0.1 > 0.1.0.
        assert!(version_newer("0.1.0.1", "0.1.0"));
    }

    #[test]
    fn ls_remote_tags_keeps_only_version_looking_refs() {
        // `--refs` (as used by check_remote/apply_now) excludes the
        // peeled `^{}` dereference lines git would otherwise print
        // alongside annotated tags, so real output never has them.
        let out = "\
abc123\trefs/tags/v0.1.0
def456\trefs/tags/v0.10.0
aaa111\trefs/tags/not-a-version
ccc333\trefs/heads/main";
        let tags = parse_ls_remote_tags(out);
        assert_eq!(tags, vec!["v0.1.0", "v0.10.0"]);
        // Numeric ordering, not lexical — v0.10.0 beats v0.1.0.
        assert_eq!(
            tags.iter().max_by_key(|t| parse_version(t)),
            Some(&"v0.10.0".to_string())
        );
    }

    #[test]
    fn ls_remote_tags_empty_on_no_tags() {
        assert!(parse_ls_remote_tags("").is_empty());
        assert!(parse_ls_remote_tags("aaa\trefs/heads/main").is_empty());
    }

    #[test]
    fn ls_remote_symref_head_parses_default_branch() {
        let out = "ref: refs/heads/main\tHEAD\nabc123def456\tHEAD\n";
        assert_eq!(parse_ls_remote_symref_head(out), Some("main".to_string()));
    }

    #[test]
    fn ls_remote_symref_head_none_without_symref_line() {
        assert_eq!(parse_ls_remote_symref_head("abc123\tHEAD\n"), None);
    }

    #[test]
    fn channel_parses_with_release_fallback() {
        assert_eq!(UpdateChannel::from_config(Some("dev")), UpdateChannel::Dev);
        assert_eq!(UpdateChannel::from_config(Some("main")), UpdateChannel::Dev);
        assert_eq!(UpdateChannel::from_config(Some("release")), UpdateChannel::Release);
        assert_eq!(UpdateChannel::from_config(Some("garbage")), UpdateChannel::Release);
        assert_eq!(UpdateChannel::from_config(None), UpdateChannel::Release);
    }

    #[test]
    fn channel_toggles_between_the_two() {
        assert_eq!(UpdateChannel::Release.toggled(), UpdateChannel::Dev);
        assert_eq!(UpdateChannel::Dev.toggled(), UpdateChannel::Release);
    }

    #[test]
    fn interval_parses_with_6h_fallback() {
        assert_eq!(UpdateInterval::from_config(Some("15m")), UpdateInterval::Min15);
        assert_eq!(UpdateInterval::from_config(Some("1h")), UpdateInterval::Hour1);
        assert_eq!(UpdateInterval::from_config(Some("6h")), UpdateInterval::Hour6);
        assert_eq!(UpdateInterval::from_config(Some("24h")), UpdateInterval::Hour24);
        assert_eq!(UpdateInterval::from_config(Some("garbage")), UpdateInterval::Hour6);
        assert_eq!(UpdateInterval::from_config(None), UpdateInterval::Hour6);
    }

    #[test]
    fn interval_cycle_visits_every_option_and_wraps() {
        let mut seen = vec![UpdateInterval::Min15];
        let mut cur = UpdateInterval::Min15;
        for _ in 0..3 {
            cur = cur.cycled();
            seen.push(cur);
        }
        assert_eq!(
            seen,
            vec![
                UpdateInterval::Min15,
                UpdateInterval::Hour1,
                UpdateInterval::Hour6,
                UpdateInterval::Hour24,
            ]
        );
        assert_eq!(cur.cycled(), UpdateInterval::Min15);
    }

    /// Scratch repo with two commits; returns (dir, older short hash).
    fn scratch_repo() -> (tempfile::TempDir, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "--quiet"]);
        run(&["commit", "--quiet", "--allow-empty", "-m", "one"]);
        let older = run(&["rev-parse", "--short", "HEAD"]);
        run(&["commit", "--quiet", "--allow-empty", "-m", "two"]);
        (tmp, older)
    }

    #[test]
    fn dev_baseline_prefers_build_commit_when_in_repo() {
        let (repo, older) = scratch_repo();
        // Build commit is an older commit still present in the repo —
        // it wins over HEAD, so being "behind" stays detectable.
        assert_eq!(dev_baseline(repo.path(), &older).unwrap(), older);
    }

    #[test]
    fn dev_baseline_falls_back_to_head_when_unknown() {
        let (repo, _) = scratch_repo();
        let head = git(repo.path(), &["rev-parse", "--short", "HEAD"]).unwrap();
        // No embedded commit (tarball build) → HEAD.
        assert_eq!(dev_baseline(repo.path(), "").unwrap(), head);
        // Embedded commit no longer exists (history rewritten) → HEAD.
        assert_eq!(dev_baseline(repo.path(), "1111111").unwrap(), head);
    }

    #[cfg(unix)]
    #[test]
    fn replace_binary_overwrites_dst_and_keeps_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("new-mewxi");
        let dst = tmp.path().join("running-mewxi");
        std::fs::write(&src, b"new").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&dst, b"old").unwrap();

        replace_binary(&src, &dst).unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "exec bits lost: {mode:o}");
        // No tempfile left behind.
        assert!(!tmp.path().join("running-mewxi.update-tmp").exists());
    }
}
