//! A status-line block: one reorderable, toggleable piece of the line.
//!
//! Blocks are TOML files (the filename stem is the default `id`). Two
//! kinds:
//!   - **Template** — a declarative `template` string (see
//!     [`super::engine`]) gated by a `when` condition.
//!   - **Command** — runs a shell `command` and shows its sanitized
//!     stdout (see [`super::command`]). Useful for git branch, cwd, etc.

use super::engine::Condition;
use anyhow::{anyhow, Result};
use serde::Deserialize;

/// Default + clamp bounds for command-block timeouts. Kept small so a
/// slow/hung command can never stall the every-~5s `mewxi status` call.
pub const DEFAULT_TIMEOUT_MS: u64 = 300;
pub const MIN_TIMEOUT_MS: u64 = 50;
pub const MAX_TIMEOUT_MS: u64 = 2000;

/// A parsed block, ready to render.
#[derive(Clone, Debug)]
pub struct Block {
    pub id: String,
    /// Human label shown in the TUI composer.
    pub label: String,
    pub kind: BlockKind,
}

#[derive(Clone, Debug)]
pub enum BlockKind {
    Template {
        when: Condition,
        template: String,
    },
    Command {
        when: Condition,
        command: String,
        /// Optional color name wrapping the whole stdout (see
        /// `engine::color_code`). `None` = uncolored.
        color: Option<String>,
        timeout_ms: u64,
    },
}

impl Block {
    /// Whether the block is a command block (shells out on render).
    pub fn is_command(&self) -> bool {
        matches!(self.kind, BlockKind::Command { .. })
    }
}

/// On-disk TOML shape. Every field is optional so a malformed/partial
/// file degrades gracefully rather than failing the whole status line.
#[derive(Deserialize)]
struct RawBlock {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Parse one block TOML. `stem` (the file name without extension) is the
/// fallback `id` and `label` when the file omits them.
pub fn parse_block(stem: &str, toml_src: &str) -> Result<Block> {
    let raw: RawBlock = toml::from_str(toml_src)?;
    let id = raw.id.unwrap_or_else(|| stem.to_string());
    let label = raw.label.unwrap_or_else(|| id.clone());
    let when = Condition::parse(raw.when.as_deref());
    let kind = match raw.kind.as_deref() {
        Some("command") => {
            let command = raw
                .command
                .filter(|c| !c.trim().is_empty())
                .ok_or_else(|| anyhow!("block '{id}' is type=command but has no `command`"))?;
            BlockKind::Command {
                when,
                command,
                color: raw.color.filter(|c| !c.trim().is_empty()),
                timeout_ms: raw
                    .timeout_ms
                    .unwrap_or(DEFAULT_TIMEOUT_MS)
                    .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
            }
        }
        Some("template") | None => BlockKind::Template {
            when,
            template: raw.template.unwrap_or_default(),
        },
        Some(other) => return Err(anyhow!("block '{id}' has unknown type '{other}'")),
    };
    Ok(Block { id, label, kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_block_round_trips() {
        let b = parse_block(
            "ctx",
            r#"
                label = "context"
                when = "ctx_present"
                template = " <cyan>ctx</cyan> {ctx_pct}"
            "#,
        )
        .unwrap();
        assert_eq!(b.id, "ctx");
        assert_eq!(b.label, "context");
        match b.kind {
            BlockKind::Template { when, template } => {
                assert_eq!(when, Condition::Flag("ctx_present".into()));
                assert_eq!(template, " <cyan>ctx</cyan> {ctx_pct}");
            }
            _ => panic!("expected template"),
        }
    }

    #[test]
    fn id_defaults_to_stem() {
        let b = parse_block("five_hour", "template = \"{five_h_segment}\"").unwrap();
        assert_eq!(b.id, "five_hour");
        assert_eq!(b.label, "five_hour");
    }

    #[test]
    fn command_block_parses_and_clamps_timeout() {
        let b = parse_block(
            "git",
            r#"
                type = "command"
                command = "git rev-parse --abbrev-ref HEAD"
                color = "green"
                timeout_ms = 99999
            "#,
        )
        .unwrap();
        match b.kind {
            BlockKind::Command {
                command,
                color,
                timeout_ms,
                ..
            } => {
                assert_eq!(command, "git rev-parse --abbrev-ref HEAD");
                assert_eq!(color.as_deref(), Some("green"));
                assert_eq!(timeout_ms, MAX_TIMEOUT_MS); // clamped
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn command_block_without_command_errors() {
        assert!(parse_block("bad", "type = \"command\"").is_err());
    }

    #[test]
    fn unknown_type_errors() {
        assert!(parse_block("bad", "type = \"widget\"").is_err());
    }
}
