//! Multi-account discovery and configuration.
//!
//! An "account" is one `CLAUDE_CONFIG_DIR` — i.e. a directory containing
//! its own `projects/` JSONL subtree and (optionally) its own credentials.
//! Users with the `claude-work` / `claude-priv` split typically point each
//! shell at a different config dir; this module makes the tool see all of
//! them.
//!
//! Discovery order:
//!  1. `~/.config/mewxi/accounts.toml` — explicit, wins when present.
//!  2. Auto-discovery: every `~/.claude*` directory containing `projects/`.
//!  3. The `CLAUDE_CONFIG_DIR` env var (when set), if it isn't already in
//!     the discovered list.
//!
//! Dedup by canonicalized `dir`, then sort by `name` for stable iteration.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// (Live-session detection is marker-driven via `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json`
// rather than JSONL mtime windows — see `live_session::scan`. The
// `live_session_*_threshold_secs` fields in `accounts.toml` are still
// parsed for backward compatibility but no longer influence detection.)

/// How `read_oauth_token` should resolve an account's bearer token.
///
/// `#[serde(untagged)]` lets the user pick exactly one variant inline:
/// `token_source = { env = "VAR" }`, `token_source = { keychain = "svc" }`,
/// or `token_source = { file = "/path" }`. Omit `token_source` to get
/// `Auto`, which mirrors Claude Code's own per-`CLAUDE_CONFIG_DIR`
/// keychain layout: try `Claude Code-credentials-{first 8 hex of
/// sha256(dir)}` first, then fall back to the legacy single-account
/// `Claude Code-credentials` entry, then to `<dir>/.credentials.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenSource {
    Env { env: String },
    Keychain { keychain: String },
    File { file: PathBuf },
    /// Construct-only; never deserialized. Triggers the fallback chain
    /// above. Skipped during ser/deser so untagged matching is unambiguous.
    #[serde(skip)]
    Auto,
}

impl Default for TokenSource {
    fn default() -> Self {
        TokenSource::Auto
    }
}

/// `Claude Code-credentials-{first 8 hex of sha256(abs_dir_path)}` —
/// the exact naming convention Claude Code uses for per-`CLAUDE_CONFIG_DIR`
/// keychain entries on macOS.
pub fn hashed_keychain_service(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(dir.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let prefix: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("Claude Code-credentials-{prefix}")
}

/// One account discovered by [`load_accounts`].
#[derive(Clone, Debug, Serialize)]
pub struct Account {
    pub name: String,
    pub dir: PathBuf,
    pub token_source: TokenSource,
}

impl Account {
    /// `<dir>/projects/`.
    pub fn projects_dir(&self) -> PathBuf {
        self.dir.join("projects")
    }

    /// Filesystem-safe slug used in per-account cache filenames.
    pub fn slug(&self) -> String {
        sanitize_slug(&self.name)
    }

    /// `<dir>/settings.json` and `<dir>/settings.local.json` for the
    /// extended-context heuristic.
    pub fn settings_paths(&self) -> [PathBuf; 2] {
        [
            self.dir.join("settings.json"),
            self.dir.join("settings.local.json"),
        ]
    }

    /// The permission mode mewxi should use as this account's startup
    /// default. Derived from `skipAutoPermissionPrompt: true` in
    /// `<dir>/settings.json`, which signals the user accepts auto mode
    /// (claude no longer asks them to confirm it each session). When
    /// set we return `auto`; otherwise `default` (claude's vanilla
    /// startup mode). `settings.local.json` overrides `settings.json`
    /// when both define the field.
    ///
    /// Note: claude itself ALWAYS launches in `default` regardless of
    /// this flag — the flag only suppresses the auto-mode confirmation
    /// prompt, not claude's startup mode. Mewxi acts on the opt-in by
    /// passing `--permission-mode auto` when spawning the child (see
    /// [`crate::agent_control::PtySession::spawn`]).
    ///
    /// Returns the raw transcript-format string so it slots into the
    /// same display path as live-scanned modes (`default` → "manual",
    /// `auto` → "auto", etc.).
    pub fn default_permission_mode(&self) -> String {
        let mut opted_in = false;
        for path in self.settings_paths() {
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(v): serde_json::Result<serde_json::Value> =
                serde_json::from_str(&raw) else { continue };
            if let Some(b) = v
                .get("skipAutoPermissionPrompt")
                .and_then(|x| x.as_bool())
            {
                opted_in = b;
            }
        }
        if opted_in { "auto".to_string() } else { "default".to_string() }
    }

    /// The default model this account uses when spawning a session
    /// without an explicit `--model` flag — derived from the `model`
    /// field in `<dir>/settings.json` (claude's standard override). If
    /// neither settings file sets it, returns `None` and the caller
    /// should fall back to claude's hardcoded default — which we render
    /// as the literal `default` placeholder, matching the picker's
    /// "Default (recommended)" option. `settings.local.json` overrides
    /// `settings.json` when both define the field.
    pub fn default_model(&self) -> Option<String> {
        let mut out: Option<String> = None;
        for path in self.settings_paths() {
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(v): serde_json::Result<serde_json::Value> =
                serde_json::from_str(&raw) else { continue };
            if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                if !m.is_empty() {
                    out = Some(m.to_string());
                }
            }
        }
        // For the default account (`~/.claude`), Claude Code's UI writes
        // the user's `model` override to `$HOME/.claude.json` rather
        // than `~/.claude/settings.json`. Fall back to that file when
        // settings.json didn't yield a value — without this, the default
        // account always returns None and the badge shows the literal
        // `default` placeholder forever.
        if out.is_none() {
            if let Some(home) = std::env::var_os("HOME") {
                let home_claude = PathBuf::from(&home).join(".claude");
                if self.dir == home_claude {
                    let path = PathBuf::from(home).join(".claude.json");
                    if let Ok(raw) = std::fs::read_to_string(&path) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                            if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                                if !m.is_empty() {
                                    out = Some(m.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The persisted `/effort` level for this account, read from
    /// `effortLevel` in `<dir>/settings.json` (or `settings.local.json`
    /// when it overrides). `None` when the user has never set one —
    /// claude then uses its built-in default ("auto").
    ///
    /// Note: Claude Code's `/effort` command is session-scoped and does
    /// not itself write this field. The value here is whatever was put
    /// there explicitly (Claude Code's own config UI, a hand-edit, or
    /// mewxi's [`set_default_effort`]).
    pub fn default_effort(&self) -> Option<String> {
        let mut out: Option<String> = None;
        for path in self.settings_paths() {
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(v): serde_json::Result<serde_json::Value> =
                serde_json::from_str(&raw) else { continue };
            if let Some(e) = v.get("effortLevel").and_then(|x| x.as_str()) {
                if !e.is_empty() {
                    out = Some(e.to_string());
                }
            }
        }
        out
    }

    /// External editor command used by the "open in editor" composer
    /// shortcut. Resolution order: `"editor"` field in
    /// `<dir>/settings.json` (or `settings.local.json` when it
    /// overrides) → `$VISUAL` → `$EDITOR` → `vim`. Returns a shell-style
    /// command line that may include args, e.g. `"code --wait"`; callers
    /// split on whitespace before spawning.
    pub fn editor_command(&self) -> String {
        let mut configured: Option<String> = None;
        for path in self.settings_paths() {
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(v): serde_json::Result<serde_json::Value> =
                serde_json::from_str(&raw) else { continue };
            if let Some(e) = v.get("editor").and_then(|x| x.as_str()) {
                if !e.trim().is_empty() {
                    configured = Some(e.to_string());
                }
            }
        }
        configured
            .or_else(|| std::env::var("VISUAL").ok().filter(|s| !s.trim().is_empty()))
            .or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "vim".to_string())
    }
}

/// Persist `level` as the `effortLevel` field of this account's
/// `<dir>/settings.json`. Creates the file (and `<dir>`) if missing,
/// preserves any other keys, and writes atomically via tempfile +
/// rename so a crash mid-write can't leave the JSON half-written.
///
/// We always target `settings.json` (not `settings.local.json`): the
/// goal is the persistent default, and `settings.local.json` is
/// per-machine-only — writing there would make the default vanish
/// when the user moves to a different host.
pub fn set_default_effort(account: &Account, level: &str) -> Result<()> {
    let path = account.dir.join("settings.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let mut root: serde_json::Value = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        if raw.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    obj.insert(
        "effortLevel".to_string(),
        serde_json::Value::String(level.to_string()),
    );
    let out = serde_json::to_string_pretty(&root)
        .with_context(|| format!("serialize {}", path.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, out)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Persist `slug` as the `model` field of this account's
/// `<dir>/settings.json` (claude's standard startup override). The
/// special slug `default` instead *removes* the field, restoring
/// claude's built-in default — mirroring the picker's "Default
/// (recommended)" row. Creates the file (and `<dir>`) if missing,
/// preserves any other keys, and writes atomically via tempfile +
/// rename. Always targets `settings.json`, never `settings.local.json`
/// (per-machine-only), for the same reason as [`set_default_effort`].
pub fn set_default_model(account: &Account, slug: &str) -> Result<()> {
    let path = account.dir.join("settings.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let mut root: serde_json::Value = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        if raw.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    if slug == "default" {
        obj.remove("model");
    } else {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(slug.to_string()),
        );
    }
    let out = serde_json::to_string_pretty(&root)
        .with_context(|| format!("serialize {}", path.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, out)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// On-disk shape of `~/.config/mewxi/accounts.toml`.
#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct AccountsConfig {
    #[serde(default)]
    default_account: Option<String>,
    /// Deprecated: detection is now marker-driven (see `live_session`).
    /// Parsed-and-ignored so old configs don't error out.
    #[serde(default)]
    live_session_threshold_secs: Option<u64>,
    #[serde(default)]
    live_session_active_threshold_secs: Option<u64>,
    #[serde(default)]
    live_session_open_threshold_secs: Option<u64>,
    /// Account names to hide from every view + aggregation. The
    /// matching `Account` records are still *discovered* so the TUI's
    /// setup view can show them as "ignored" and offer to un-ignore.
    #[serde(default)]
    ignored: Vec<String>,
    /// Starting folder for the new-session modal. `~` is expanded.
    /// Env override: `MEWXI_DEFAULT_NEW_SESSION_DIR`. Falls back to
    /// `$HOME` when neither is set.
    #[serde(default)]
    default_new_session_dir: Option<String>,
    /// When true, the driver input row loses focus after a prompt is
    /// sent — keys then route to view navigation instead of typing into
    /// the next prompt. Default: true.
    #[serde(default)]
    defocus_input_after_send: Option<bool>,
    /// Which view the TUI opens on: `"overview"` (default),
    /// `"session"`, `"account"`, or `"config"`. The view's number key
    /// (`"1"`–`"4"`) is accepted too. Ignored on first run while setup
    /// is incomplete — the setup view wins then.
    #[serde(default)]
    default_view: Option<String>,
    /// Self-update channel: `"release"` (tagged versions, the default)
    /// or `"dev"` (follow origin's main branch). Interpreted by
    /// [`crate::update::UpdateChannel::from_config`].
    #[serde(default)]
    update_channel: Option<String>,
    /// When false, mewxi never checks origin for updates on its own —
    /// no TUI startup check, no watch-daemon refresh. Manual checks
    /// (`mewxi update`, the Config view's "check for updates") still
    /// work. Default: true.
    #[serde(default)]
    update_check: Option<bool>,
    /// Minimum time between automatic update checks: `"15m"`, `"1h"`,
    /// `"6h"` (default) or `"24h"`. Interpreted by
    /// [`crate::update::UpdateInterval::from_config`].
    #[serde(default)]
    update_interval: Option<String>,
    /// When true (default), the TUI checks for updates on startup and
    /// asks before installing one.
    #[serde(default)]
    update_prompt: Option<bool>,
    /// Where the mewxi source checkout lives, for self-update. Only
    /// needed when the checkout moved after the binary was built —
    /// the compile-time manifest dir is used otherwise. `~` expands.
    #[serde(default)]
    update_repo_dir: Option<String>,
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize)]
struct AccountEntry {
    name: String,
    dir: String,
    #[serde(default)]
    token_source: Option<TokenSource>,
}

/// Result of [`load_accounts`] — accounts plus tunables from `accounts.toml`.
#[derive(Debug, Clone)]
pub struct AccountsView {
    /// Active (non-ignored) accounts. This is the list every "normal"
    /// consumer (TUI views 1/2/3, `dump`, MCP) iterates.
    pub accounts: Vec<Account>,
    /// Accounts discovered on disk but marked as ignored by the
    /// user's `accounts.toml`. The setup view shows these alongside
    /// active accounts so the user can toggle them back on.
    pub ignored: Vec<Account>,
    pub default_account: Option<String>,
    /// Starting folder for the new-session modal (config-supplied,
    /// pre-tilde-expansion not applied — see [`default_new_session_dir`]).
    pub default_new_session_dir: Option<PathBuf>,
    /// When true, the driver input row auto-unfocuses after a prompt is
    /// sent. Toggleable from the Config view. Defaults to true.
    pub defocus_input_after_send: bool,
    /// Raw `default_view` value from `accounts.toml`; parsed by the
    /// TUI's `ViewMode::from_config`. `None` = overview.
    pub default_view: Option<String>,
    /// Raw `update_channel` value from `accounts.toml`; parsed by
    /// [`crate::update::UpdateChannel::from_config`]. `None` = release.
    pub update_channel: Option<String>,
    /// Check origin for updates automatically (TUI startup, watch
    /// daemon). Manual checks ignore this. Defaults to true.
    pub update_check: bool,
    /// Raw `update_interval` value from `accounts.toml`; parsed by
    /// [`crate::update::UpdateInterval::from_config`]. `None` = 6h.
    pub update_interval: Option<String>,
    /// Ask about available updates on TUI startup. Defaults to true.
    pub update_prompt: bool,
    /// Optional override for the self-update source checkout location.
    pub update_repo_dir: Option<PathBuf>,
}

impl AccountsView {
    /// Lookup a named account; falls back to `default_account`, then the
    /// first entry. Returns None only if the active list is empty.
    /// Only searches *active* accounts — never returns an ignored one.
    pub fn pick(&self, name: Option<&str>) -> Option<&Account> {
        if let Some(n) = name {
            if let Some(a) = self.accounts.iter().find(|a| a.name == n) {
                return Some(a);
            }
        }
        if let Some(ref n) = self.default_account {
            if let Some(a) = self.accounts.iter().find(|a| &a.name == n) {
                return Some(a);
            }
        }
        self.accounts.first()
    }

    /// Iterate active + ignored together in stable order. Used by the
    /// setup view so the user sees every account on disk.
    pub fn all_accounts(&self) -> impl Iterator<Item = (&Account, bool)> {
        self.accounts.iter().map(|a| (a, false))
            .chain(self.ignored.iter().map(|a| (a, true)))
    }

    pub fn ignored_names(&self) -> Vec<String> {
        self.ignored.iter().map(|a| a.name.clone()).collect()
    }
}

/// Path to `accounts.toml`. We intentionally use the XDG-style
/// `~/.config/mewxi/` on every platform (including macOS)
/// rather than `dirs::config_dir()` (which on macOS returns
/// `~/Library/Application Support`). This matches the path documented
/// in the README and keeps the config location predictable for
/// developers used to XDG conventions. `$XDG_CONFIG_HOME` overrides.
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("mewxi").join("accounts.toml"))
}

/// Discover accounts; never fails just because the config file is absent.
pub fn load_accounts() -> Result<AccountsView> {
    let mut accounts: Vec<Account> = Vec::new();
    let mut default_account: Option<String> = None;
    let mut ignored_names: Vec<String> = Vec::new();
    let mut default_new_session_dir: Option<PathBuf> = None;
    let mut defocus_input_after_send: bool = true;
    let mut default_view: Option<String> = None;
    let mut update_channel: Option<String> = None;
    let mut update_check: bool = true;
    let mut update_interval: Option<String> = None;
    let mut update_prompt: bool = true;
    let mut update_repo_dir: Option<PathBuf> = None;

    if let Some(cfg_path) = config_path() {
        if cfg_path.exists() {
            let raw = std::fs::read_to_string(&cfg_path)
                .with_context(|| format!("read {}", cfg_path.display()))?;
            let cfg: AccountsConfig = toml::from_str(&raw)
                .with_context(|| format!("parse {}", cfg_path.display()))?;
            default_account = cfg.default_account;
            ignored_names = cfg.ignored;
            default_new_session_dir = cfg.default_new_session_dir.map(|s| expand_tilde(&s));
            if let Some(v) = cfg.defocus_input_after_send {
                defocus_input_after_send = v;
            }
            default_view = cfg.default_view;
            update_channel = cfg.update_channel;
            if let Some(v) = cfg.update_check {
                update_check = v;
            }
            update_interval = cfg.update_interval;
            if let Some(v) = cfg.update_prompt {
                update_prompt = v;
            }
            update_repo_dir = cfg.update_repo_dir.map(|s| expand_tilde(&s));
            for entry in cfg.accounts {
                accounts.push(Account {
                    name: entry.name,
                    dir: expand_tilde(&entry.dir),
                    token_source: entry.token_source.unwrap_or_default(),
                });
            }
        }
    }

    if accounts.is_empty() {
        accounts.extend(auto_discover());
    }

    if let Some(env_dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let env_path = PathBuf::from(env_dir);
        if !accounts
            .iter()
            .any(|a| same_path(&a.dir, &env_path))
        {
            let name = name_from_dir(&env_path);
            accounts.push(Account {
                name,
                dir: env_path,
                token_source: TokenSource::default(),
            });
        }
    }

    dedup_by_canonical_path(&mut accounts);
    accounts.sort_by(|a, b| a.name.cmp(&b.name));

    if accounts.is_empty() {
        return Err(anyhow!(
            "no Claude config directories discovered. \
             Either create ~/.config/mewxi/accounts.toml, \
             ensure ~/.claude (or ~/.claude-*) exists, \
             or set CLAUDE_CONFIG_DIR."
        ));
    }

    // Split into active vs ignored by name.
    let ignored_set: std::collections::HashSet<&str> =
        ignored_names.iter().map(|s| s.as_str()).collect();
    let (ignored_vec, active): (Vec<Account>, Vec<Account>) = accounts
        .into_iter()
        .partition(|a| ignored_set.contains(a.name.as_str()));

    Ok(AccountsView {
        accounts: active,
        ignored: ignored_vec,
        default_account,
        default_new_session_dir,
        defocus_input_after_send,
        default_view,
        update_channel,
        update_check,
        update_interval,
        update_prompt,
        update_repo_dir,
    })
}

/// A directory the account has worked in before, annotated with how
/// many resumable transcripts live under it and when the newest was
/// last touched. Powers the new-session modal's Recent pane so the
/// "do I want to resume something here?" question can be answered at a
/// glance, before drilling into the folder.
#[derive(Clone, Debug)]
pub struct RecentProject {
    /// The working directory (recovered from the transcript's `cwd`).
    pub dir: PathBuf,
    /// Number of `.jsonl` transcripts under this project dir — i.e. how
    /// many sessions are resumable here.
    pub session_count: usize,
    /// Modification time of the newest transcript in the folder.
    pub latest_mtime: std::time::SystemTime,
}

/// Directories this account has been used in before, newest first,
/// each annotated with its resumable-session count and latest activity.
///
/// Discovers history by scanning `<account.dir>/projects/<encoded>/*.jsonl`.
/// Claude Code's `projects/<encoded>` dir-name encoding is lossy
/// (it flattens `/`, `_`, and `.` all to `-`), so we recover the real
/// cwd by parsing one record out of each project's transcripts —
/// every record carries the unescaped `cwd` field. The session count
/// is the cheap `.jsonl` file tally we already walk, so this is no more
/// expensive than reading the dirs alone. Sorted by the newest JSONL
/// mtime per project, dedup'd by canonical path, capped at `limit`.
pub fn recent_projects(account: &Account, limit: usize) -> Vec<RecentProject> {
    let projects = account.projects_dir();
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    // (cwd, newest mtime, transcript count) per encoded project dir.
    let mut found: Vec<(PathBuf, std::time::SystemTime, usize)> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
        let mut count = 0usize;
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            count += 1;
            let Ok(meta) = f.metadata() else { continue };
            let Ok(mt) = meta.modified() else { continue };
            if newest.as_ref().is_none_or(|(_, t)| mt > *t) {
                newest = Some((p, mt));
            }
        }
        let Some((jsonl, mtime)) = newest else { continue };
        let Some((cwd, _)) = read_cwd_and_preview(&jsonl) else { continue };
        found.push((cwd, mtime, count));
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out: Vec<RecentProject> = Vec::with_capacity(found.len().min(limit));
    let mut seen: Vec<PathBuf> = Vec::new();
    for (cwd, mtime, count) in found {
        let key = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
        if seen.iter().any(|p| p == &key) {
            continue;
        }
        seen.push(key);
        out.push(RecentProject {
            dir: cwd,
            session_count: count,
            latest_mtime: mtime,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Resumable sessions whose recorded `cwd` matches `dir` (canonical
/// equality), newest first. Used by the new-session modal's Sessions
/// pane to show what's resumable under the currently-browsed folder.
pub fn sessions_in_dir(account: &Account, dir: &Path, limit: usize) -> Vec<RecentSession> {
    let canon_target = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut out: Vec<RecentSession> = recent_project_sessions(account, limit * 4)
        .into_iter()
        .filter(|s| {
            let c = std::fs::canonicalize(&s.cwd).unwrap_or_else(|_| s.cwd.clone());
            c == canon_target
        })
        .collect();
    out.truncate(limit);
    out
}

/// One resumable session discovered by [`recent_project_sessions`].
#[derive(Clone, Debug)]
pub struct RecentSession {
    /// File-stem of the JSONL — what claude expects after `--resume`.
    pub session_id: String,
    /// Working directory recorded in the transcript.
    pub cwd: PathBuf,
    /// Last modification time of the JSONL.
    pub mtime: std::time::SystemTime,
    /// First user message (truncated). None when the transcript has
    /// only system/envelope frames.
    pub preview: Option<String>,
}

/// Resumable sessions for this account, newest first.
///
/// Each `<account.dir>/projects/<encoded>/<session_id>.jsonl` is one
/// session whose ID is the file stem. Returns up to `limit` entries,
/// sorted by mtime descending. Reads each JSONL's first ~128 lines to
/// extract `cwd` and the earliest user-message text for preview.
pub fn recent_project_sessions(account: &Account, limit: usize) -> Vec<RecentSession> {
    let projects = account.projects_dir();
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            let Ok(mt) = meta.modified() else { continue };
            found.push((p, mt));
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out: Vec<RecentSession> = Vec::with_capacity(found.len().min(limit));
    for (jsonl, mtime) in found.into_iter().take(limit * 2) {
        let Some(session_id) = jsonl.file_stem().and_then(|s| s.to_str()).map(String::from)
        else {
            continue;
        };
        let Some((cwd, preview)) = read_cwd_and_preview(&jsonl) else { continue };
        out.push(RecentSession { session_id, cwd, mtime, preview });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Scan up to ~128 lines of `jsonl`, extracting `cwd` and the first
/// user-message text. Returns `None` when no `cwd` is found.
fn read_cwd_and_preview(path: &Path) -> Option<(PathBuf, Option<String>)> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(f);
    let mut cwd: Option<PathBuf> = None;
    let mut preview: Option<String> = None;
    for line in reader.lines().take(128).flatten() {
        let Ok(v): serde_json::Result<serde_json::Value> =
            serde_json::from_str(&line) else { continue };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|s| s.as_str()) {
                if !c.is_empty() {
                    cwd = Some(PathBuf::from(c));
                }
            }
        }
        if preview.is_none() && v.get("type").and_then(|t| t.as_str()) == Some("user") {
            if let Some(text) = extract_user_text(&v) {
                let trimmed = text.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('<') {
                    preview = Some(truncate_chars(trimmed, 80));
                }
            }
        }
        if cwd.is_some() && preview.is_some() {
            break;
        }
    }
    cwd.map(|c| (c, preview))
}

/// Pull the first text block out of a `type=user` transcript record.
/// Tolerates both stringly-typed `content` and the array-of-blocks form.
fn extract_user_text(v: &serde_json::Value) -> Option<String> {
    let msg = v.get("message")?;
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|s| s.as_str()) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            return out;
        }
        if ch == '\n' || ch == '\r' || ch == '\t' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Resolve the directory the new-session modal should open in.
///
/// Order: `MEWXI_DEFAULT_NEW_SESSION_DIR` env var (if it points to an
/// existing directory) → `default_new_session_dir` in
/// `accounts.toml` → `$HOME` → `/`.
pub fn resolve_default_new_session_dir(view: &AccountsView) -> PathBuf {
    if let Some(env) = std::env::var_os("MEWXI_DEFAULT_NEW_SESSION_DIR") {
        let p = expand_tilde(&env.to_string_lossy());
        if p.is_dir() {
            return p;
        }
    }
    if let Some(p) = view.default_new_session_dir.as_ref() {
        if p.is_dir() {
            return p.clone();
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Load `accounts.toml` (or an empty table), let `mutate` edit the
/// top-level table, and write the result back atomically (tempfile +
/// rename). Creates the file and parent dirs if missing. Every
/// single-field setter below funnels through here so they all preserve
/// unrelated content the same way.
fn edit_config_table(mutate: impl FnOnce(&mut toml::value::Table)) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow!("no XDG config dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut root: toml::Value = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        if raw.trim().is_empty() {
            toml::Value::Table(toml::value::Table::new())
        } else {
            toml::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        toml::Value::Table(toml::value::Table::new())
    };
    let toml::Value::Table(t) = &mut root else {
        return Err(anyhow!("{} is not a TOML table", path.display()));
    };
    mutate(t);
    let out = toml::to_string_pretty(&root)
        .with_context(|| format!("serialize {}", path.display()))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Write the `ignored = [...]` field to `accounts.toml`, preserving
/// any other content. Creates the file if it doesn't exist. Pass an
/// empty slice to clear the ignore list.
pub fn set_ignored(names: &[String]) -> Result<()> {
    edit_config_table(|t| {
        if names.is_empty() {
            t.remove("ignored");
        } else {
            let arr: Vec<toml::Value> =
                names.iter().map(|s| toml::Value::String(s.clone())).collect();
            t.insert("ignored".to_string(), toml::Value::Array(arr));
        }
    })
}

/// Write the `defocus_input_after_send` field to `accounts.toml`,
/// preserving any other content. Creates the file if it doesn't exist.
pub fn set_defocus_input_after_send(enabled: bool) -> Result<()> {
    edit_config_table(|t| {
        t.insert(
            "defocus_input_after_send".to_string(),
            toml::Value::Boolean(enabled),
        );
    })
}

/// Persist which view opens when the TUI starts
/// (`"all"` / `"session"` / `"account"` / `"config"`).
pub fn set_default_view(view: &str) -> Result<()> {
    edit_config_table(|t| {
        t.insert(
            "default_view".to_string(),
            toml::Value::String(view.to_string()),
        );
    })
}

/// Persist the self-update channel (`"release"` / `"dev"`).
pub fn set_update_channel(channel: &str) -> Result<()> {
    edit_config_table(|t| {
        t.insert(
            "update_channel".to_string(),
            toml::Value::String(channel.to_string()),
        );
    })
}

/// Persist whether mewxi checks origin for updates automatically.
pub fn set_update_check(enabled: bool) -> Result<()> {
    edit_config_table(|t| {
        t.insert("update_check".to_string(), toml::Value::Boolean(enabled));
    })
}

/// Persist the minimum time between automatic update checks
/// (`"15m"` / `"1h"` / `"6h"` / `"24h"`).
pub fn set_update_interval(interval: &str) -> Result<()> {
    edit_config_table(|t| {
        t.insert(
            "update_interval".to_string(),
            toml::Value::String(interval.to_string()),
        );
    })
}

/// Persist whether the TUI asks about updates on startup.
pub fn set_update_prompt(enabled: bool) -> Result<()> {
    edit_config_table(|t| {
        t.insert("update_prompt".to_string(), toml::Value::Boolean(enabled));
    })
}

/// Toggle the ignore flag for `name`. Returns the new ignored state
/// (true = now ignored, false = now active).
pub fn toggle_ignored(name: &str) -> Result<bool> {
    let view = load_accounts()?;
    let mut current = view.ignored_names();
    let was_ignored = current.iter().any(|n| n == name);
    if was_ignored {
        current.retain(|n| n != name);
    } else {
        current.push(name.to_string());
        current.sort();
        current.dedup();
    }
    set_ignored(&current)?;
    Ok(!was_ignored)
}

/// Find which account a given transcript belongs to, by checking which
/// account's `dir` is an ancestor of the path. Tolerates a non-existent
/// transcript path (so this still works when called from a hook that
/// passes a path before the file is flushed to disk).
pub fn account_for_transcript<'a>(
    accounts: &'a [Account],
    transcript: &Path,
) -> Option<&'a Account> {
    let canon = std::fs::canonicalize(transcript).unwrap_or_else(|_| transcript.to_path_buf());
    accounts.iter().find(|a| {
        let adir = std::fs::canonicalize(&a.dir).unwrap_or_else(|_| a.dir.clone());
        canon.starts_with(&adir) || transcript.starts_with(&a.dir)
    })
}

fn auto_discover() -> Vec<Account> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&home) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else { continue };
        if !fname.starts_with(".claude") {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if !path.join("projects").is_dir() {
            continue;
        }
        let name = name_from_dir(&path);
        out.push(Account {
            name,
            dir: path,
            token_source: TokenSource::default(),
        });
    }
    out
}

/// `.claude` → `default`, `.claude-work` → `work`, `.claude-priv` → `priv`.
fn name_from_dir(dir: &Path) -> String {
    let fname = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("account");
    let trimmed = fname.trim_start_matches('.');
    let after_claude = trimmed.strip_prefix("claude").unwrap_or(trimmed);
    let after_sep = after_claude
        .strip_prefix('-')
        .or_else(|| after_claude.strip_prefix('_'))
        .unwrap_or(after_claude);
    if after_sep.is_empty() {
        "default".to_string()
    } else {
        after_sep.to_string()
    }
}

fn dedup_by_canonical_path(accounts: &mut Vec<Account>) {
    let mut seen: Vec<PathBuf> = Vec::with_capacity(accounts.len());
    accounts.retain(|a| {
        let canon = std::fs::canonicalize(&a.dir).unwrap_or_else(|_| a.dir.clone());
        if seen.iter().any(|p| p == &canon) {
            false
        } else {
            seen.push(canon);
            true
        }
    });
}

fn same_path(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

fn sanitize_slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn account_in(dir: &Path) -> Account {
        Account {
            name: "test".into(),
            dir: dir.to_path_buf(),
            token_source: TokenSource::default(),
        }
    }

    #[test]
    fn set_default_effort_creates_settings_when_missing() {
        let tmp = TempDir::new().unwrap();
        let acc = account_in(tmp.path());
        set_default_effort(&acc, "max").unwrap();
        assert_eq!(acc.default_effort().as_deref(), Some("max"));
    }

    #[test]
    fn set_default_effort_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"claude-opus-4-7","skipAutoPermissionPrompt":true}"#,
        )
        .unwrap();
        let acc = account_in(tmp.path());
        set_default_effort(&acc, "high").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["effortLevel"], "high");
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["skipAutoPermissionPrompt"], true);
    }

    #[test]
    fn set_default_effort_overwrites_existing_value() {
        let tmp = TempDir::new().unwrap();
        let acc = account_in(tmp.path());
        set_default_effort(&acc, "low").unwrap();
        set_default_effort(&acc, "high").unwrap();
        assert_eq!(acc.default_effort().as_deref(), Some("high"));
    }
}
