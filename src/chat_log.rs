//! Read a Claude Code transcript JSONL into a flat list of chat entries
//! suitable for rendering. We intentionally skip plumbing like
//! `file-history-snapshot`, meta system messages, and empty `thinking`
//! blocks that only carry a redaction signature — they're noise in a
//! chat view. Tool calls and tool results are kept and summarised on
//! one or two lines each.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Clone, Debug)]
pub enum EntryKind {
    User,
    Assistant,
    Thinking,
    ToolUse { name: String, input: Value },
    ToolResult { ok: bool },
    System,
}

#[derive(Clone, Debug)]
pub struct ChatEntry {
    #[allow(dead_code)]
    pub ts: Option<DateTime<Utc>>,
    pub kind: EntryKind,
    /// Plain text body. For tool_use/tool_result this is a short
    /// summary; for user/assistant it's the joined text content.
    pub text: String,
}

pub fn read(path: &Path) -> Vec<ChatEntry> {
    let Ok(f) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(f);
    let mut out: Vec<ChatEntry> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        parse_record(&v, &mut out);
    }
    out
}

fn parse_record(v: &Value, out: &mut Vec<ChatEntry>) {
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let is_meta = v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false);
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match typ {
        "user" => {
            let content = match v.get("message").and_then(|m| m.get("content")) {
                Some(c) => c,
                None => return,
            };
            // `content` is either a plain string (a user prompt) or an
            // array of blocks that may include `tool_result` items.
            if let Some(s) = content.as_str() {
                let text = clean_user_text(s);
                if text.is_empty() || is_meta {
                    return;
                }
                out.push(ChatEntry { ts, kind: EntryKind::User, text });
            } else if let Some(arr) = content.as_array() {
                for item in arr {
                    let kind = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match kind {
                        "tool_result" => {
                            let is_err = item
                                .get("is_error")
                                .and_then(|b| b.as_bool())
                                .unwrap_or(false);
                            let body = tool_result_text(item.get("content"));
                            out.push(ChatEntry {
                                ts,
                                kind: EntryKind::ToolResult { ok: !is_err },
                                text: body,
                            });
                        }
                        "text" => {
                            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                                let text = clean_user_text(t);
                                if !text.is_empty() {
                                    out.push(ChatEntry {
                                        ts,
                                        kind: EntryKind::User,
                                        text,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        "assistant" => {
            let arr = match v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                Some(a) => a,
                None => return,
            };
            for item in arr {
                let kind = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                out.push(ChatEntry {
                                    ts,
                                    kind: EntryKind::Assistant,
                                    text: trimmed.to_string(),
                                });
                            }
                        }
                    }
                    "thinking" => {
                        if let Some(t) = item.get("thinking").and_then(|x| x.as_str()) {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                out.push(ChatEntry {
                                    ts,
                                    kind: EntryKind::Thinking,
                                    text: trimmed.to_string(),
                                });
                            }
                        }
                    }
                    "tool_use" => {
                        let name = item
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let input = item.get("input").cloned().unwrap_or(Value::Null);
                        out.push(ChatEntry {
                            ts,
                            kind: EntryKind::ToolUse { name, input },
                            text: String::new(),
                        });
                    }
                    _ => {}
                }
            }
        }
        "system" => {
            if is_meta {
                return;
            }
            let text = v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !text.is_empty() {
                out.push(ChatEntry {
                    ts,
                    kind: EntryKind::System,
                    text,
                });
            }
        }
        _ => {}
    }
}

/// Strip Claude Code's local-command markers and other XML-ish noise
/// that clutters a chat view. We keep the visible text the human
/// actually typed/saw.
fn clean_user_text(s: &str) -> String {
    const BLOCK_TAGS: &[&str] = &[
        "<local-command-caveat>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<system-reminder>",
    ];

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut in_tag = false;

    'outer: while !rest.is_empty() {
        if !in_tag {
            for tag in BLOCK_TAGS {
                if rest.starts_with(tag) {
                    let after = &rest[tag.len()..];
                    let close = closing_tag(tag);
                    match after.find(close) {
                        Some(p) => {
                            rest = &after[p + close.len()..];
                        }
                        None => {
                            // Unterminated — drop the rest entirely.
                            return out.trim().to_string();
                        }
                    }
                    continue 'outer;
                }
            }
        }
        // Advance by one char so slicing stays on a valid boundary.
        let c = rest.chars().next().unwrap();
        let step = c.len_utf8();
        rest = &rest[step..];
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn closing_tag(open: &str) -> &'static str {
    match open {
        "<local-command-caveat>" => "</local-command-caveat>",
        "<command-name>" => "</command-name>",
        "<command-message>" => "</command-message>",
        "<command-args>" => "</command-args>",
        "<local-command-stdout>" => "</local-command-stdout>",
        "<local-command-stderr>" => "</local-command-stderr>",
        "<system-reminder>" => "</system-reminder>",
        _ => "",
    }
}

/// One-line summary of a tool_use input so the chat view stays scannable.
pub fn tool_input_summary(name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    // A short list of fields we care about, in priority order. First
    // match wins.
    const KEYS: &[&str] = &[
        "command", "file_path", "path", "pattern", "query", "prompt", "url", "description",
    ];
    for k in KEYS {
        if let Some(s) = input.get(*k).and_then(|v| v.as_str()) {
            return one_line(s, 120);
        }
    }
    // Fallback: stringify the whole input, truncated.
    let s = serde_json::to_string(input).unwrap_or_default();
    let _ = name;
    one_line(&s, 120)
}

fn tool_result_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(arr) = c.as_array() {
        // Concatenate every text block so the changes panel can show
        // the full output. Inline rendering will collapse this to one
        // line via `one_line`.
        let mut parts: Vec<&str> = Vec::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                parts.push(t);
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    String::new()
}

pub fn one_line(s: &str, max: usize) -> String {
    let collapsed: String = s
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    let mut chars: Vec<char> = collapsed.chars().collect();
    if chars.len() > max {
        chars.truncate(max);
        let mut t: String = chars.into_iter().collect();
        t.push('…');
        t
    } else {
        chars.into_iter().collect()
    }
}
