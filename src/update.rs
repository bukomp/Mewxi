//! Self-update: check the source checkout's git remote for a newer
//! mewxi and rebuild via `cargo install`.
//!
//! mewxi is installed from a git checkout (`cargo install --path .`),
//! so "update" means: fetch the remote, compare, fast-forward the
//! checkout, and re-run the install. Two channels, picked in
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
//! `CARGO_MANIFEST_DIR` (exactly right for `cargo install --path .`),
//! overridable with `update_repo_dir` in `accounts.toml` when the
//! checkout has moved since the build.
//!
//! Every successful check is cached at
//! `~/.cache/mewxi/update-check.json` so cheap consumers — most
//! importantly the statusLine renderer that runs inside every Claude
//! Code session — can show an "update available" notice without
//! touching the network. The cache is refreshed by the TUI on startup
//! and by the `watch` daemon every few hours. Both automatic checks
//! honor `update_check = false` in `accounts.toml`; explicit checks
//! (`mewxi update`, the Config view row) always run.

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
    let raw = std::fs::read_to_string(path).ok()?;
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
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
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

/// Locate the source checkout this binary was built from. Compile-time
/// `CARGO_MANIFEST_DIR` is authoritative for `cargo install --path .`;
/// `update_repo_dir` in `accounts.toml` overrides it when the checkout
/// has moved since the build.
pub fn repo_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    let dir = override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
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

/// Run `git -C <repo> <args>` and return trimmed stdout. Errors carry
/// stderr so failures (auth, network) read meaningfully in the TUI.
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit this binary was built from, embedded by `build.rs`.
/// Empty when the build had no git context (e.g. a source tarball).
const BUILD_COMMIT: &str = env!("MEWXI_BUILD_COMMIT");

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
/// contexts. Writes the cache on success.
pub fn check_now() -> Result<UpdateStatus> {
    let view = accounts::load_accounts()?;
    let channel = channel_from_view(&view);
    let repo = repo_dir(view.update_repo_dir.as_deref())?;
    git(&repo, &["fetch", "--quiet", "--tags", "origin"]).context("fetching origin")?;

    let status = match channel {
        UpdateChannel::Release => {
            let current = env!("CARGO_PKG_VERSION").to_string();
            match latest_version_tag(&repo)? {
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
            let branch = remote_default_branch(&repo);
            let current = dev_baseline(&repo, BUILD_COMMIT)?;
            let behind: u64 = git(&repo, &["rev-list", "--count", &format!("{current}..origin/{branch}")])?
                .parse()
                .unwrap_or(0);
            let latest = git(&repo, &["rev-parse", "--short", &format!("origin/{branch}")])?;
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
    };
    write_cache(&status);
    Ok(status)
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

/// Update the checkout to the channel's newest point and rebuild via
/// `cargo install --path <repo> --force`. Streams git/cargo output to
/// the inherited stdout/stderr — call only with the terminal restored
/// (the TUI suspends its alternate screen around this). Refuses to
/// touch a dirty checkout.
pub fn apply_now() -> Result<String> {
    let view = accounts::load_accounts()?;
    let channel = channel_from_view(&view);
    let repo = repo_dir(view.update_repo_dir.as_deref())?;

    let dirty = git(&repo, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(anyhow!(
            "{} has uncommitted changes — update it manually (git pull && cargo install --path .)",
            repo.display()
        ));
    }

    println!("mewxi update ({})", channel.as_str());
    println!("  checkout: {}", repo.display());
    println!("  fetching origin …");
    git(&repo, &["fetch", "--quiet", "--tags", "origin"]).context("fetching origin")?;

    let target = match channel {
        UpdateChannel::Release => {
            let tag = latest_version_tag(&repo)?
                .ok_or_else(|| anyhow!("no version tags on origin — switch to the dev channel?"))?;
            println!("  checking out {tag} …");
            git(&repo, &["checkout", "--quiet", &tag])?;
            tag
        }
        UpdateChannel::Dev => {
            let branch = remote_default_branch(&repo);
            println!("  fast-forwarding {branch} …");
            git(&repo, &["checkout", "--quiet", &branch])?;
            git(&repo, &["merge", "--ff-only", "--quiet", &format!("origin/{branch}")])
                .context("fast-forward failed (local commits on the branch?)")?;
            git(&repo, &["rev-parse", "--short", "HEAD"])?
        }
    };

    println!("  building (cargo install --path … --force) — this can take a minute …");
    let status = Command::new("cargo")
        .arg("install")
        .arg("--path")
        .arg(&repo)
        .arg("--force")
        .status()
        .context("running cargo install (is cargo on PATH?)")?;
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
        let is_dev_build = running.starts_with(repo.join("target"));
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
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .ok()
        .filter(|p| p.is_file())
        .unwrap_or_else(cargo_bin_path);
    Command::new(exe).args(std::env::args_os().skip(1)).exec()
}

/// Where `cargo install` puts binaries: `$CARGO_INSTALL_ROOT`, then
/// `$CARGO_HOME`, then `~/.cargo` — each with `/bin/mewxi` appended.
fn cargo_bin_path() -> PathBuf {
    let root = std::env::var_os("CARGO_INSTALL_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))
        .unwrap_or_default();
    root.join("bin").join("mewxi")
}

/// Atomically replace `dst` with a copy of `src` (copy to a sibling
/// tempfile, then rename). The running process keeps its old inode, so
/// overwriting a live executable's path is safe on unix.
fn replace_binary(src: &Path, dst: &Path) -> Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".update-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::copy(src, &tmp)
        .with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, dst) {
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
    Some(format!(
        " \x1b[90m|\x1b[0m \x1b[35m⬆ mewxi {what}\x1b[0m"
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
