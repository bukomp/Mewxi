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

    if let Some(cfg_path) = config_path() {
        if cfg_path.exists() {
            let raw = std::fs::read_to_string(&cfg_path)
                .with_context(|| format!("read {}", cfg_path.display()))?;
            let cfg: AccountsConfig = toml::from_str(&raw)
                .with_context(|| format!("parse {}", cfg_path.display()))?;
            default_account = cfg.default_account;
            ignored_names = cfg.ignored;
            default_new_session_dir = cfg.default_new_session_dir.map(|s| expand_tilde(&s));
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
    })
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

/// Write the `ignored = [...]` field to `accounts.toml`, preserving
/// any other content. Creates the file if it doesn't exist. Pass an
/// empty slice to clear the ignore list.
pub fn set_ignored(names: &[String]) -> Result<()> {
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
    if let toml::Value::Table(t) = &mut root {
        if names.is_empty() {
            t.remove("ignored");
        } else {
            let arr: Vec<toml::Value> = names.iter().map(|s| toml::Value::String(s.clone())).collect();
            t.insert("ignored".to_string(), toml::Value::Array(arr));
        }
    } else {
        return Err(anyhow!("{} is not a TOML table", path.display()));
    }
    let out = toml::to_string_pretty(&root)
        .with_context(|| format!("serialize {}", path.display()))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
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
