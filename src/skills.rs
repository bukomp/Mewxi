//! Discover the skills + slash commands installed for a Claude Code
//! account, so mewxi can surface them in a picker before injecting the
//! chosen `/<name>` invocation into the driven PTY.
//!
//! Mirrors the locations Claude Code itself scans (per its public docs
//! at <https://code.claude.com/docs/en/skills.md> and
//! <https://code.claude.com/docs/en/plugins.md>):
//!
//! - **User scope** — `<CLAUDE_CONFIG_DIR>/skills/<name>/SKILL.md` and
//!   `<CLAUDE_CONFIG_DIR>/commands/<name>.md`.
//! - **Project scope** — walks up from the session's working directory
//!   to its repo root (or filesystem root if there's no `.git`), looking
//!   for `.claude/skills/<name>/SKILL.md` and `.claude/commands/<name>.md`
//!   at each level.
//! - **Plugins** — reads
//!   `<CLAUDE_CONFIG_DIR>/plugins/installed_plugins.json` (the v2 file
//!   the live `claude` binary actually consults — the older
//!   `installed.json` is ignored), then under each plugin's `installPath`
//!   scans `skills/<name>/SKILL.md` and `commands/<name>.md`.
//!
//! Plugin-bundled entries surface as `<plugin>:<name>` to match the
//! `frontend-design:frontend-design` form Claude Code shows the model;
//! user/project entries surface as the bare `<name>`. The `name:` field
//! from a SKILL.md frontmatter is display-only — Claude Code derives the
//! command from the *directory* (skills) or *filename* (commands), and
//! we follow the same rule.
//!
//! `skillOverrides` from settings.json drops `"off"` entries entirely
//! and blanks the description for `"name-only"`. `"user-invocable-only"`
//! is kept as-is because this picker *is* the user-invocable surface.
//!
//! Frontmatter parsing is intentionally tiny: we don't pull in a YAML
//! crate. SKILL/command frontmatter only needs the `description` field,
//! which is a single line (sometimes folded onto continuation lines).
//! A 50-line regex-free scanner is enough and keeps the dependency
//! footprint flat.

use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// One discovered skill or slash command that the user can pick from
/// the mewxi picker. `name` is what gets sent to the PTY as
/// `/<name>\r` — including any `plugin:` prefix for plugin-bundled
/// entries.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub origin: SkillOrigin,
    /// Path to the SKILL.md or commands/*.md file. Carried for
    /// diagnostics; the picker shows it in a footer line on selection.
    pub source_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillOrigin {
    User,
    Project,
    /// Plugin name (without `@marketplace` suffix), e.g. `pr-review-toolkit`.
    Plugin(String),
    /// Compiled into the `claude` binary itself — extracted by scanning
    /// the executable file for skill-registration patterns. Best-effort:
    /// a future claude release that changes the bundle layout would
    /// silently drop these from the picker without breaking anything else.
    BuiltIn,
}

impl SkillOrigin {
    pub fn label(&self) -> &str {
        match self {
            SkillOrigin::User => "user",
            SkillOrigin::Project => "project",
            SkillOrigin::Plugin(_) => "plugin",
            SkillOrigin::BuiltIn => "built-in",
        }
    }
}

/// Discover every invocable skill/command for an account+cwd, deduped
/// and sorted alphabetically by `name`. Walking errors (missing dirs,
/// unreadable files) are ignored — discovery is best-effort and never
/// blocks the picker.
///
/// `claude_bin`, when `Some`, points at the `claude` executable to
/// scan for built-in skills. Pass `None` to skip the binary scan
/// entirely (filesystem skills only).
pub fn discover(
    config_dir: &Path,
    project_cwd: &Path,
    claude_bin: Option<&Path>,
) -> Vec<Skill> {
    let overrides = load_skill_overrides(config_dir);
    let mut out: Vec<Skill> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let push = |skill: Skill, seen: &mut HashSet<String>, out: &mut Vec<Skill>| {
        match overrides.get(skill.name.as_str()).map(String::as_str) {
            Some("off") => return,
            Some("name-only") => {
                let mut s = skill;
                s.description.clear();
                if seen.insert(s.name.clone()) {
                    out.push(s);
                }
            }
            _ => {
                if seen.insert(skill.name.clone()) {
                    out.push(skill);
                }
            }
        }
    };

    // Project scope first so it wins dedup against same-named user/plugin
    // entries — matches Claude Code's "closer scope overrides farther"
    // precedence. (Plugin entries are namespaced and never collide.)
    for skill in scan_project_scope(project_cwd) {
        push(skill, &mut seen, &mut out);
    }
    for skill in scan_user_scope(config_dir) {
        push(skill, &mut seen, &mut out);
    }
    for skill in scan_plugin_scope(config_dir) {
        push(skill, &mut seen, &mut out);
    }
    if let Some(bin) = claude_bin {
        for skill in scan_builtin_scope(bin) {
            push(skill, &mut seen, &mut out);
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Best-effort extraction of built-in skill registrations from the
/// `claude` binary. The CLI bundles its skills as compiled JS objects;
/// each registration looks roughly like:
///
/// ```text
/// nY({name:"<name>",description:"<desc>",...userInvocable:!0,...})
/// ```
///
/// We look for `{name:"<NAME>","` and confirm the same record contains
/// `userInvocable:!0` within a kilobyte. If the binary's minifier
/// renames things or shifts layout in a future release this just
/// returns an empty Vec — the FS-based skills still cover the picker.
///
/// Resolves symlinks so a `~/.local/bin/claude` shim that points at
/// `versions/<v>/claude` still reads the actual ELF.
fn scan_builtin_scope(claude_bin: &Path) -> Vec<Skill> {
    let resolved = fs::canonicalize(claude_bin).unwrap_or_else(|_| claude_bin.to_path_buf());
    let Ok(bytes) = fs::read(&resolved) else {
        return Vec::new();
    };
    extract_builtin_skills(&bytes, &resolved)
}

/// Pure-function half of [`scan_builtin_scope`] so we can unit-test the
/// extractor against a synthetic buffer without needing a real binary
/// on disk.
fn extract_builtin_skills(bytes: &[u8], source: &Path) -> Vec<Skill> {
    const NAME_PREFIX: &[u8] = b"{name:\"";
    const DESC_PREFIX: &[u8] = b",description:\"";
    const USER_INVOCABLE: &[u8] = b"userInvocable:!0";
    // 8 KiB lookahead is enough to bracket the largest registration
    // we've observed (large descriptions push close to ~4 KiB).
    const LOOKAHEAD: usize = 8 * 1024;

    let mut out: Vec<Skill> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i + NAME_PREFIX.len() < bytes.len() {
        let Some(rel) = find_subslice(&bytes[i..], NAME_PREFIX) else { break };
        let start = i + rel + NAME_PREFIX.len();
        let Some(end_quote) = find_closing_quote(&bytes[start..]) else {
            i = start;
            continue;
        };
        let name = match std::str::from_utf8(&bytes[start..start + end_quote]) {
            Ok(s) if is_valid_skill_name(s) => s.to_string(),
            _ => {
                i = start + end_quote + 1;
                continue;
            }
        };
        // Tail starts immediately after the closing quote. Cap it at
        // the lookahead window OR the start of the *next* `{name:"`
        // record, whichever comes first — otherwise a registration that
        // lacks `userInvocable:!0` would falsely inherit the marker
        // from the registration that follows it in the bundle.
        let tail_start = start + end_quote + 1;
        let mut tail_end = (tail_start + LOOKAHEAD).min(bytes.len());
        if let Some(next) = find_subslice(&bytes[tail_start..tail_end], NAME_PREFIX) {
            tail_end = tail_start + next;
        }
        let tail = &bytes[tail_start..tail_end];
        let user_invocable = find_subslice(tail, USER_INVOCABLE).is_some();
        if !user_invocable {
            i = tail_start;
            continue;
        }
        let description = find_subslice(tail, DESC_PREFIX)
            .and_then(|p| {
                let desc_start = p + DESC_PREFIX.len();
                find_closing_quote(&tail[desc_start..]).map(|e| {
                    String::from_utf8_lossy(&tail[desc_start..desc_start + e]).into_owned()
                })
            })
            .map(unescape_js)
            .unwrap_or_default();
        if seen.insert(name.clone()) {
            out.push(Skill {
                name,
                description,
                origin: SkillOrigin::BuiltIn,
                source_path: source.to_path_buf(),
            });
        }
        i = tail_start;
    }
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Index of the closing unescaped `"` in a JS string literal that
/// starts immediately after an opening `"`. Returns `None` if no
/// terminator is found within the buffer.
fn find_closing_quote(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            // JS string literals can't contain a raw newline; this
            // catches false matches that happen to align with other
            // binary data containing `{name:"...`.
            b'\n' | b'\r' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Minimal JS string unescaping for the descriptions we extract.
/// Handles `\"`, `\\`, `\n`, `\t` — enough for the human-readable text
/// claude actually stores. Unknown escapes are passed through unchanged.
fn unescape_js(s: String) -> String {
    if !s.contains('\\') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Quick filter to rule out the many `{name:"..."` literals that show
/// up in unrelated bundled code (CSS attrs, AWS SDK definitions, etc.)
/// Built-in skill names are short kebab/identifier strings.
fn is_valid_skill_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn scan_user_scope(config_dir: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    scan_skills_dir(&config_dir.join("skills"), SkillOrigin::User, &mut out);
    scan_commands_dir(&config_dir.join("commands"), SkillOrigin::User, &mut out);
    out
}

fn scan_project_scope(cwd: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let mut cur: Option<&Path> = Some(cwd);
    while let Some(dir) = cur {
        let claude_dir = dir.join(".claude");
        if claude_dir.is_dir() {
            scan_skills_dir(&claude_dir.join("skills"), SkillOrigin::Project, &mut out);
            scan_commands_dir(&claude_dir.join("commands"), SkillOrigin::Project, &mut out);
        }
        // Stop climbing once we hit a `.git/` (repo root) — going past
        // that risks picking up siblings of unrelated repos. If no
        // `.git/` exists anywhere, the loop naturally terminates at
        // the filesystem root.
        if dir.join(".git").exists() {
            break;
        }
        cur = dir.parent();
    }
    out
}

fn scan_plugin_scope(config_dir: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let installed = config_dir.join("plugins").join("installed_plugins.json");
    let Ok(raw) = fs::read_to_string(&installed) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return out;
    };
    let Some(plugins) = v.get("plugins").and_then(Value::as_object) else {
        return out;
    };
    for (plugin_key, entries) in plugins {
        // `plugins` is a map of `<plugin>@<marketplace>` to an array of
        // install records (one per scope). Each record has an
        // `installPath` that points at the on-disk copy.
        let plugin_name = plugin_key.split('@').next().unwrap_or(plugin_key).to_string();
        let Some(arr) = entries.as_array() else { continue };
        for entry in arr {
            let Some(install_path) = entry.get("installPath").and_then(Value::as_str) else {
                continue;
            };
            let install = PathBuf::from(install_path);
            scan_skills_dir(
                &install.join("skills"),
                SkillOrigin::Plugin(plugin_name.clone()),
                &mut out,
            );
            scan_commands_dir(
                &install.join("commands"),
                SkillOrigin::Plugin(plugin_name.clone()),
                &mut out,
            );
        }
    }
    out
}

fn scan_skills_dir(dir: &Path, origin: SkillOrigin, out: &mut Vec<Skill>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let description = read_description(&skill_md).unwrap_or_default();
        out.push(Skill {
            name: prefix_name(&origin, dir_name),
            description,
            origin: origin.clone(),
            source_path: skill_md,
        });
    }
}

fn scan_commands_dir(dir: &Path, origin: SkillOrigin, out: &mut Vec<Skill>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        let description = read_description(&path).unwrap_or_default();
        out.push(Skill {
            name: prefix_name(&origin, stem),
            description,
            origin: origin.clone(),
            source_path: path,
        });
    }
}

fn prefix_name(origin: &SkillOrigin, base: &str) -> String {
    match origin {
        SkillOrigin::Plugin(p) => format!("{p}:{base}"),
        _ => base.to_string(),
    }
}

/// Pull the `description` field out of the YAML frontmatter at the top
/// of a SKILL.md / command .md file. Returns `None` when the file has
/// no frontmatter, the field is missing, or I/O fails.
///
/// Handles three frontmatter shapes:
///
/// ```text
/// description: short single-line value
/// description: "quoted value"
/// description: >
///   folded
///   multi-line
///   value
/// ```
pub fn read_description(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut value: Option<String> = None;
    let mut continuation: Option<String> = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some(cont) = continuation.as_mut() {
            // Continuation of a `>` / `|` folded value: collect any
            // line that starts with whitespace, stop at the next
            // top-level key.
            if line.starts_with(' ') || line.starts_with('\t') {
                if !cont.is_empty() {
                    cont.push(' ');
                }
                cont.push_str(line.trim());
                continue;
            }
            value = Some(std::mem::take(cont));
            continuation = None;
        }
        if let Some(rest) = line.strip_prefix("description:") {
            let trimmed = rest.trim();
            if trimmed == ">" || trimmed == "|" {
                continuation = Some(String::new());
            } else {
                value = Some(strip_quotes(trimmed).to_string());
            }
        }
    }
    if let Some(cont) = continuation {
        value = Some(cont);
    }
    value.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Load `skillOverrides` from settings.json + settings.local.json,
/// merging local on top of global. Keys are skill names (as the user
/// would invoke them — `plugin:name` for plugin entries); values are
/// `"on"` / `"off"` / `"name-only"` / `"user-invocable-only"`.
fn load_skill_overrides(config_dir: &Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for fname in ["settings.json", "settings.local.json"] {
        let p = config_dir.join(fname);
        let Ok(raw) = fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else { continue };
        let Some(obj) = v.get("skillOverrides").and_then(Value::as_object) else { continue };
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn reads_inline_description() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("SKILL.md");
        write(&skill, "---\nname: foo\ndescription: A short one-liner.\n---\n\nBody.\n");
        assert_eq!(
            read_description(&skill).as_deref(),
            Some("A short one-liner.")
        );
    }

    #[test]
    fn reads_quoted_description() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("SKILL.md");
        write(&skill, "---\ndescription: \"With quotes\"\n---\n");
        assert_eq!(read_description(&skill).as_deref(), Some("With quotes"));
    }

    #[test]
    fn reads_folded_multiline_description() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("SKILL.md");
        write(
            &skill,
            "---\nname: foo\ndescription: >\n  Line one\n  line two.\nname: foo\n---\n",
        );
        assert_eq!(
            read_description(&skill).as_deref(),
            Some("Line one line two.")
        );
    }

    #[test]
    fn missing_frontmatter_returns_none() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("SKILL.md");
        write(&skill, "# No frontmatter here\n");
        assert!(read_description(&skill).is_none());
    }

    #[test]
    fn discovers_user_skills_and_commands() {
        let cfg = tempdir().unwrap();
        let project = tempdir().unwrap();

        // User skill via skills/<name>/SKILL.md
        let skill_md = cfg.path().join("skills/deploy/SKILL.md");
        write(&skill_md, "---\ndescription: Deploy the thing.\n---\n");

        // User command via commands/<name>.md
        let cmd_md = cfg.path().join("commands/ship.md");
        write(&cmd_md, "---\ndescription: Ship it.\n---\n");

        let skills = discover(cfg.path(), project.path(), None);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"deploy"), "got {names:?}");
        assert!(names.contains(&"ship"), "got {names:?}");

        let deploy = skills.iter().find(|s| s.name == "deploy").unwrap();
        assert_eq!(deploy.origin, SkillOrigin::User);
        assert_eq!(deploy.description, "Deploy the thing.");
    }

    #[test]
    fn discovers_plugin_skills_with_prefixed_names() {
        let cfg = tempdir().unwrap();
        let project = tempdir().unwrap();

        // Plugin skill on disk: <cfg>/plugins/cache/.../skills/review/SKILL.md
        let plugin_root = cfg
            .path()
            .join("plugins/cache/official/pr-review-toolkit/1.0.0");
        let skill_md = plugin_root.join("skills/review/SKILL.md");
        write(&skill_md, "---\ndescription: Review a PR.\n---\n");

        // installed_plugins.json pointing at it. Build via serde_json so the
        // installPath is escaped correctly on Windows (backslash paths would
        // otherwise produce invalid JSON escapes).
        let installed = cfg.path().join("plugins/installed_plugins.json");
        let manifest = serde_json::json!({
            "version": 2,
            "plugins": {
                "pr-review-toolkit@official": [{
                    "scope": "user",
                    "installPath": plugin_root.to_str().unwrap(),
                    "version": "1.0.0"
                }]
            }
        });
        write(&installed, &serde_json::to_string_pretty(&manifest).unwrap());

        let skills = discover(cfg.path(), project.path(), None);
        let review = skills
            .iter()
            .find(|s| s.name == "pr-review-toolkit:review")
            .expect("plugin skill should appear with prefix");
        assert_eq!(review.origin, SkillOrigin::Plugin("pr-review-toolkit".into()));
        assert_eq!(review.description, "Review a PR.");
    }

    #[test]
    fn skill_overrides_off_drops_entry() {
        let cfg = tempdir().unwrap();
        let project = tempdir().unwrap();

        write(
            &cfg.path().join("skills/legacy/SKILL.md"),
            "---\ndescription: Old.\n---\n",
        );
        write(
            &cfg.path().join("settings.json"),
            r#"{"skillOverrides": {"legacy": "off"}}"#,
        );

        let skills = discover(cfg.path(), project.path(), None);
        assert!(skills.iter().all(|s| s.name != "legacy"));
    }

    #[test]
    fn skill_overrides_name_only_blanks_description() {
        let cfg = tempdir().unwrap();
        let project = tempdir().unwrap();

        write(
            &cfg.path().join("skills/quiet/SKILL.md"),
            "---\ndescription: Secret.\n---\n",
        );
        write(
            &cfg.path().join("settings.json"),
            r#"{"skillOverrides": {"quiet": "name-only"}}"#,
        );

        let s = discover(cfg.path(), project.path(), None);
        let quiet = s.iter().find(|s| s.name == "quiet").unwrap();
        assert_eq!(quiet.description, "");
    }

    /// Local smoke-check against the user's real `~/.claude`. Skipped by
    /// default — run with `cargo test --bin mewxi -- --ignored smoke`.
    #[test]
    #[ignore]
    fn smoke_against_real_config() {
        let home = dirs::home_dir().unwrap();
        let cfg = home.join(".claude");
        let cwd = std::env::current_dir().unwrap();
        let bin = home.join(".local/bin/claude");
        let bin_opt = bin.exists().then_some(bin.as_path());
        let skills = discover(&cfg, &cwd, bin_opt);
        eprintln!("found {} skills/commands:", skills.len());
        for s in &skills {
            eprintln!(
                "  {:40} [{}] — {}",
                s.name,
                s.origin.label(),
                s.description.chars().take(80).collect::<String>()
            );
        }
        assert!(!skills.is_empty(), "expected to find at least one skill");
    }

    #[test]
    fn extracts_builtins_from_synthetic_blob() {
        // Mimics the bundled-claude shape: name + description + the
        // userInvocable:!0 marker somewhere within ~1 KiB of the name.
        let payload = b"\x00\x00garbage\x00function nY(){}\x00\
            nY({name:\"my-skill\",description:\"Short text.\",allowedTools:[\"Read\"],userInvocable:!0,async\
            getPromptForCommand(){}})\x00\
            {name:\"not-a-skill\",description:\"Has no userInvocable marker.\"}\x00\
            nY({name:\"another\",description:\"Second \\\"quoted\\\" one.\",userInvocable:!0})\x00";
        let extracted = extract_builtin_skills(payload, Path::new("/fake/claude"));
        let names: Vec<&str> = extracted.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"my-skill"), "got {names:?}");
        assert!(names.contains(&"another"), "got {names:?}");
        assert!(!names.contains(&"not-a-skill"), "got {names:?}");
        let my = extracted.iter().find(|s| s.name == "my-skill").unwrap();
        assert_eq!(my.origin, SkillOrigin::BuiltIn);
        assert_eq!(my.description, "Short text.");
        let another = extracted.iter().find(|s| s.name == "another").unwrap();
        assert_eq!(another.description, "Second \"quoted\" one.");
    }

    #[test]
    fn builtin_extractor_rejects_garbage_names() {
        let payload = b"{name:\"FooBar\",description:\"caps\",userInvocable:!0}\
            {name:\"with space\",description:\"spaces\",userInvocable:!0}\
            {name:\"ok-name\",description:\"valid\",userInvocable:!0}";
        let extracted = extract_builtin_skills(payload, Path::new("/fake"));
        let names: Vec<&str> = extracted.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["ok-name"], "got {names:?}");
    }

    #[test]
    fn project_scope_walks_up_to_repo_root() {
        let cfg = tempdir().unwrap();
        let project = tempdir().unwrap();
        let root = project.path();
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        // mark root as a repo so the walk-up terminates here
        fs::create_dir_all(root.join(".git")).unwrap();
        write(
            &root.join(".claude/skills/local/SKILL.md"),
            "---\ndescription: Local one.\n---\n",
        );

        let skills = discover(cfg.path(), &nested, None);
        let local = skills
            .iter()
            .find(|s| s.name == "local")
            .expect("project skill should be found from nested cwd");
        assert_eq!(local.origin, SkillOrigin::Project);
    }
}
