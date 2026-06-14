//! Status-line rendering context + template engine.
//!
//! [`Ctx`] precomputes exactly the same intermediate segment strings the
//! legacy `watch::render_status_for_account` built (5h meter, reset,
//! context, extra-usage, model/thinking, account prefix, the update +
//! setup nudges) by reusing the byte-exact helpers in [`crate::watch`].
//! Those intermediates are exposed as named **fields** that block
//! templates interpolate via `{field}`. Because the computation is the
//! original code — only the *composition order* moved out to data — the
//! default block set reproduces today's line byte-for-byte (guarded by
//! the golden tests in [`super`]).
//!
//! Template grammar:
//!   - `{field}` — substitute a field's value (absent → nothing).
//!   - `<color>…</color>` — wrap literal text in an ANSI color; nestable.
//!     Colors: cyan, grey/gray, yellow, magenta, red, green, blue, white.
//!   - everything else is literal.
//!
//! Fields whose value already carries its own ANSI (percentages,
//! whole-segment fields) must NOT be wrapped in a `<color>` tag — their
//! embedded reset would close the tag early.

use crate::accounts::Account;
use crate::watch::{self, SessionMeta};
use crate::{live_usage, setup, stats, update};
use std::collections::HashMap;
use std::path::Path;

/// A `when = "…"` visibility predicate parsed from a block's TOML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// `when` omitted, empty, or `"always"`.
    Always,
    /// A named flag (see [`Ctx::flag`]).
    Flag(String),
    /// `when = "!flag"`.
    Not(Box<Condition>),
}

impl Condition {
    /// Parse a raw `when` string. Leading `!` negates the named flag.
    pub fn parse(s: Option<&str>) -> Condition {
        match s.map(str::trim) {
            None | Some("") | Some("always") => Condition::Always,
            Some(flag) => match flag.strip_prefix('!') {
                Some(rest) => Condition::Not(Box::new(Condition::Flag(rest.trim().to_string()))),
                None => Condition::Flag(flag.to_string()),
            },
        }
    }
}

/// Everything a block can read for one `mewxi status` invocation. Built
/// once per render via [`Ctx::build`].
pub struct Ctx {
    multi_account: bool,
    billing_extra: bool,
    fields: HashMap<&'static str, String>,
}

impl Ctx {
    /// Load the aggregate + live usage for `account`, then compute every
    /// field via [`Ctx::from_data`]. The normal entry point for
    /// `mewxi status`.
    pub fn build(
        account: &Account,
        multi_account: bool,
        transcript: Option<&Path>,
        meta: SessionMeta<'_>,
        no_live: bool,
    ) -> Ctx {
        let agg = stats::load_and_aggregate_for(account).unwrap_or_default();
        let live = live_usage::fetch_or_cached(account, no_live);
        Ctx::from_data(account, multi_account, transcript, meta, &agg, live.as_ref())
    }

    /// Compute every field + flag from already-loaded data, mirroring the
    /// legacy `watch::render_status_for_account` body exactly (same branch
    /// order, same helpers). Split out from [`Ctx::build`] so the golden
    /// byte-identity tests can feed fixed `agg`/`live` and compare against
    /// the legacy renderer deterministically. Also performs the
    /// per-session persistence side effects (`mark_extended_context` /
    /// `mark_session_effort`) the statusline path has always done.
    pub fn from_data(
        account: &Account,
        multi_account: bool,
        transcript: Option<&Path>,
        meta: SessionMeta<'_>,
        agg: &stats::Aggregate,
        live: Option<&live_usage::LiveUsage>,
    ) -> Ctx {
        // --- extra-usage promotion (identical to watch.rs) ----------------
        let five_h_at_cap = live
            .as_ref()
            .and_then(|l| l.five_hour.as_ref())
            .is_some_and(|w| w.utilization >= 100.0);
        let billing_extra = five_h_at_cap
            && live
                .as_ref()
                .and_then(|l| l.extra_usage.as_ref())
                .filter(|e| e.is_enabled)
                .and_then(|e| e.used_credits)
                .is_some_and(|c| c > 0.0);

        // --- 5h window + reset (same branch as the legacy renderer) -------
        let (five_h_segment, reset_segment) = if billing_extra {
            (String::new(), watch::five_h_reset_from_live(live))
        } else {
            match watch::five_h_from_live(live) {
                Some((seg, reset)) => (seg, reset),
                None => watch::local_five_h_segment(agg),
            }
        };

        // --- per-session persistence side effects -------------------------
        let session_id = transcript
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(str::to_string);
        if let (Some(alias), Some(sid)) = (meta.model_alias, session_id.as_deref()) {
            if alias.contains("[1m]") {
                stats::mark_extended_context(account, sid);
            }
        }
        if let (Some(eff), Some(sid)) = (
            meta.effort_level.filter(|s| !s.is_empty()),
            session_id.as_deref(),
        ) {
            stats::mark_session_effort(account, sid, eff);
        }

        // --- context pieces (granular, byte-identical to ctx_segment) -----
        let ctx = transcript
            .and_then(stats::current_context_from_transcript)
            .map(|sc| {
                let cap = stats::context_cap_for(
                    &sc.model,
                    sc.max_observed,
                    meta.model_alias,
                    account,
                    session_id.as_deref(),
                );
                let pct = (sc.current as f64 / cap as f64 * 100.0).min(999.0);
                let color = watch::pct_color(pct);
                (
                    format!("\x1b[{c}m{p:.0}%\x1b[0m", c = color, p = pct),
                    watch::fmt_tokens_compact(sc.current),
                    watch::fmt_tokens_compact(cap),
                )
            });

        // --- extra-usage pieces (only when actually billing) --------------
        let extra = if billing_extra {
            live
                .and_then(|l| l.extra_usage.as_ref())
                .map(|e| {
                    let used = e.used_credits.unwrap_or(0.0) / 100.0;
                    let limit = e.monthly_limit.unwrap_or(0.0) / 100.0;
                    let pct = e.utilization.unwrap_or(0.0);
                    let color = watch::pct_color(pct);
                    let sym = watch::currency_symbol(e.currency.as_deref());
                    (
                        format!("\x1b[{c}m{p:.1}%\x1b[0m", c = color, p = pct),
                        format!(
                            "\x1b[90m({sym}{:.2}/{sym}{:.2})\x1b[0m",
                            used, limit
                        ),
                    )
                })
        } else {
            None
        };

        // --- model + thinking ---------------------------------------------
        let model_name = match meta.model_display {
            Some(name) if !name.is_empty() => Some(watch::compact_model_name(name)),
            _ => None,
        };
        let think = if meta.thinking_enabled {
            let lvl = meta.effort_level.filter(|s| !s.is_empty()).unwrap_or("on");
            format!(" \x1b[90m·\x1b[0m \x1b[35mthink:{lvl}\x1b[0m")
        } else {
            String::new()
        };

        // --- nudges (already fully-rendered ANSI segments) ----------------
        let update_segment = update::statusline_segment();
        let hint_segment = if setup::setup_incomplete() {
            Some(
                "\x1b[33m⚠ mewxi: setup incomplete — open mewxi\x1b[0m \x1b[90m|\x1b[0m "
                    .to_string(),
            )
        } else {
            None
        };

        // --- assemble the field map ---------------------------------------
        let mut fields: HashMap<&'static str, String> = HashMap::new();
        if let Some(h) = hint_segment {
            fields.insert("hint_segment", h);
        }
        if let Some(u) = update_segment {
            fields.insert("update_segment", u);
        }
        fields.insert("account", account.name.clone());
        if let Some(m) = model_name {
            fields.insert("model", m);
        }
        fields.insert("think", think); // present even when empty
        if !five_h_segment.is_empty() {
            fields.insert("five_h_segment", five_h_segment);
        }
        if !reset_segment.is_empty() {
            fields.insert("reset_segment", reset_segment);
        }
        if let Some((pct, amounts)) = extra {
            fields.insert("extra_pct", pct);
            fields.insert("extra_amounts", amounts);
        }
        if let Some((pct, cur, cap)) = ctx {
            fields.insert("ctx_pct", pct);
            fields.insert("ctx_cur", cur);
            fields.insert("ctx_cap", cap);
        }

        Ctx {
            multi_account,
            billing_extra,
            fields,
        }
    }

    /// Construct a `Ctx` directly from a field map + flags. Used by the
    /// TUI composer to render a representative preview without live data.
    pub fn from_parts(
        multi_account: bool,
        billing_extra: bool,
        fields: HashMap<&'static str, String>,
    ) -> Ctx {
        Ctx {
            multi_account,
            billing_extra,
            fields,
        }
    }

    /// Value of a `{field}`, or `None` when absent.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }

    /// Evaluate a parsed `when` predicate.
    pub fn eval_when(&self, c: &Condition) -> bool {
        match c {
            Condition::Always => true,
            Condition::Not(inner) => !self.eval_when(inner),
            Condition::Flag(name) => self.flag(name),
        }
    }

    /// Named visibility flags usable in `when`. Unknown names are `false`
    /// (a block guarded by a typo'd flag stays hidden rather than break
    /// the line).
    fn flag(&self, name: &str) -> bool {
        match name {
            "always" => true,
            "multi_account" => self.multi_account,
            "model_present" => self.fields.contains_key("model"),
            "five_h_visible" => !self.billing_extra,
            "billing_extra" => self.billing_extra,
            "reset_present" => self.fields.contains_key("reset_segment"),
            "ctx_present" => self.fields.contains_key("ctx_pct"),
            "update_available" => self.fields.contains_key("update_segment"),
            "setup_incomplete" => self.fields.contains_key("hint_segment"),
            _ => false,
        }
    }
}

/// Render one template block: empty string when `when` is false,
/// otherwise the interpolated + color-expanded template.
pub fn render_template_block(ctx: &Ctx, when: &Condition, template: &str) -> String {
    if !ctx.eval_when(when) {
        return String::new();
    }
    render_template(ctx, template)
}

/// Map a `<color>` tag name to its ANSI SGR code. Shared with command
/// blocks, which wrap their stdout in the named color.
pub(crate) fn color_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "cyan" => "36",
        "grey" | "gray" => "90",
        "yellow" => "33",
        "magenta" => "35",
        "red" => "31",
        "green" => "32",
        "blue" => "34",
        "white" => "37",
        _ => return None,
    })
}

/// Interpolate `{field}` placeholders and expand `<color>…</color>` spans.
/// A `<color>` open pushes its SGR code; the matching close emits a reset
/// and re-applies the enclosing color (so nesting works). Unrecognized
/// `{`/`<` sequences are emitted literally.
fn render_template(ctx: &Ctx, template: &str) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len() + 16);
    let mut stack: Vec<&'static str> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if let Some(close) = find_from(&chars, i + 1, '}') {
                let name: String = chars[i + 1..close].iter().collect();
                if let Some(v) = ctx.field(name.trim()) {
                    out.push_str(v);
                }
                i = close + 1;
                continue;
            }
        } else if c == '<' {
            if let Some(close) = find_from(&chars, i + 1, '>') {
                let inner: String = chars[i + 1..close].iter().collect();
                let (is_close, name) = match inner.strip_prefix('/') {
                    Some(rest) => (true, rest.trim()),
                    None => (false, inner.trim()),
                };
                if let Some(code) = color_code(name) {
                    if is_close {
                        if stack.pop().is_some() {
                            out.push_str("\x1b[0m");
                            if let Some(top) = stack.last() {
                                out.push_str("\x1b[");
                                out.push_str(top);
                                out.push('m');
                            }
                        }
                    } else {
                        out.push_str("\x1b[");
                        out.push_str(code);
                        out.push('m');
                        stack.push(code);
                    }
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    if !stack.is_empty() {
        out.push_str("\x1b[0m");
    }
    out
}

fn find_from(chars: &[char], start: usize, needle: char) -> Option<usize> {
    chars[start..].iter().position(|&c| c == needle).map(|p| start + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(pairs: &[(&'static str, &str)], multi: bool, billing: bool) -> Ctx {
        let fields = pairs.iter().map(|(k, v)| (*k, v.to_string())).collect();
        Ctx::from_parts(multi, billing, fields)
    }

    #[test]
    fn color_tag_expands_to_exact_bytes() {
        let ctx = ctx_with(&[], false, false);
        assert_eq!(
            render_template(&ctx, "<cyan>5h</cyan>"),
            "\x1b[36m5h\x1b[0m"
        );
        assert_eq!(
            render_template(&ctx, " <grey>|</grey> "),
            " \x1b[90m|\x1b[0m "
        );
    }

    #[test]
    fn field_substitution_and_absence() {
        let ctx = ctx_with(&[("account", "priv")], false, false);
        assert_eq!(
            render_template(&ctx, "<magenta>[{account}]</magenta> "),
            "\x1b[35m[priv]\x1b[0m "
        );
        // Absent field interpolates to nothing.
        assert_eq!(render_template(&ctx, "x{missing}y"), "xy");
    }

    #[test]
    fn when_false_hides_block() {
        let ctx = ctx_with(&[("account", "priv")], false, false);
        let out = render_template_block(
            &ctx,
            &Condition::Flag("multi_account".into()),
            "<magenta>[{account}]</magenta> ",
        );
        assert_eq!(out, "");
    }

    #[test]
    fn condition_parse_negation() {
        assert_eq!(Condition::parse(None), Condition::Always);
        assert_eq!(Condition::parse(Some("always")), Condition::Always);
        assert_eq!(
            Condition::parse(Some("ctx_present")),
            Condition::Flag("ctx_present".into())
        );
        assert_eq!(
            Condition::parse(Some("!billing_extra")),
            Condition::Not(Box::new(Condition::Flag("billing_extra".into())))
        );
    }

    #[test]
    fn nested_colors_restore_outer() {
        let ctx = ctx_with(&[], false, false);
        assert_eq!(
            render_template(&ctx, "<grey>(<cyan>x</cyan>)</grey>"),
            "\x1b[90m(\x1b[36mx\x1b[0m\x1b[90m)\x1b[0m"
        );
    }

    #[test]
    fn unknown_tag_is_literal() {
        let ctx = ctx_with(&[], false, false);
        assert_eq!(render_template(&ctx, "a < b"), "a < b");
        assert_eq!(render_template(&ctx, "<nope>x</nope>"), "<nope>x</nope>");
    }
}
