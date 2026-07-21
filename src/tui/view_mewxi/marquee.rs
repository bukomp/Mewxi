//! Live ticker content for the rave view's marquee row.
//!
//! The rave view is mostly vibes — bars bouncing, a pixel-font HUD — but
//! the marquee is where it earns its keep: a scrolling one-liner that
//! answers "what are my agents actually doing right now?" without
//! forcing a trip back to the table view. It surfaces the two things
//! worth interrupting a glance for — a session stuck waiting on you
//! (NEEDS INPUT), and what every other working session is up to, plus
//! how many sub-agents it has fanned out and what the lead one is
//! saying. Idle sessions are noise here and are dropped entirely; when
//! nothing qualifies, the ticker falls back to the gag string this
//! module replaced.

use crate::live_session::{Activity, SessionState};
use crate::tui::SessionRef;

/// Segment cap — [`compose`] keeps at most this many items before
/// collapsing the remainder into a single `+k more` segment, so the
/// ticker can't grow unboundedly wide with a huge fleet.
const MAX_SEGMENTS: usize = 6;

/// Caption length cap, in `char`s (not bytes) — keeps a chatty
/// sub-agent narration from dominating the line.
const CAPTION_MAX_CHARS: usize = 40;

/// Fallback line shown when no session needs input or is working.
const IDLE_TEXT: &str = "agents.exe · idle — press n to spawn an agent";

/// One top-level session's worth of ticker-relevant facts, already
/// reduced from a `SessionRef` + its sub-agent rows. Kept separate from
/// `SessionRef` so the composition logic (below) is testable without
/// constructing mewxi's ~25-field session type.
struct TickerItem {
    project: String,
    activity: String,
    needs_input: bool,
    working: bool,
    subagents: usize,
    caption: Option<String>,
}

/// Pick the best available caption for a session's lead sub-agent row,
/// in precedence order: live status label, then narration, then the
/// static launch description. Blank/whitespace-only values are treated
/// as absent and fall through to the next source. The result is
/// truncated to at most [`CAPTION_MAX_CHARS`] `char`s with a trailing
/// `…` when it was longer.
fn best_caption(
    status_label: Option<&str>,
    narration: Option<&str>,
    description: &str,
) -> Option<String> {
    let raw = [status_label, narration, Some(description)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())?;

    let char_count = raw.chars().count();
    if char_count <= CAPTION_MAX_CHARS {
        Some(raw.to_string())
    } else {
        let truncated: String = raw.chars().take(CAPTION_MAX_CHARS).collect();
        Some(format!("{truncated}…"))
    }
}

/// Render one item into its marquee segment (rule 6), with no leading
/// `⚠`/separator handling — that's [`compose`]'s job.
fn render_segment(item: &TickerItem) -> String {
    if item.needs_input {
        return format!("⚠ {} NEEDS INPUT", item.project);
    }
    let mut seg = format!("{} » {}", item.project, item.activity);
    if item.subagents > 0 {
        seg.push_str(&format!(" +{}⚡", item.subagents));
    }
    if let Some(caption) = &item.caption {
        seg.push_str(" · ");
        seg.push_str(caption);
    }
    seg
}

/// Build the final marquee string from already-extracted items,
/// implementing the ordering, cap, and fallback rules. Pure — this is
/// the logic the unit tests below exercise directly, without touching
/// `SessionRef`.
fn compose(items: &[TickerItem]) -> String {
    let mut needs_input: Vec<&TickerItem> = items.iter().filter(|i| i.needs_input).collect();
    let mut working: Vec<&TickerItem> =
        items.iter().filter(|i| !i.needs_input && i.working).collect();
    // Both filters already preserve slice order; nothing left to sort —
    // just concatenate needs-input ahead of working.
    needs_input.append(&mut working);
    let ordered = needs_input;

    if ordered.is_empty() {
        return IDLE_TEXT.to_string();
    }

    let total = ordered.len();
    let shown = ordered.iter().take(MAX_SEGMENTS).map(|i| render_segment(i));
    let mut segments: Vec<String> = shown.collect();
    if total > MAX_SEGMENTS {
        segments.push(format!("+{} more", total - MAX_SEGMENTS));
    }

    segments.join(" ░ ")
}

/// Build the marquee line for this frame's sessions. Adapter over
/// [`compose`]: walks the flattened `sessions` slice, keeps only
/// top-level, non-killing rows (rule 1), counts each one's live
/// sub-agents and picks a caption from the first of them in slice order
/// (rules 2, 6), and drops sessions that neither need input nor are
/// working (rules 3-5).
pub fn ticker_text(sessions: &[&SessionRef]) -> String {
    let items: Vec<TickerItem> = sessions
        .iter()
        .filter(|s| s.subagent.is_none() && !s.killing)
        .filter_map(|s| {
            let needs_input = matches!(s.activity, Activity::Asking | Activity::Awaiting);
            let working = s.state == SessionState::Active;
            if !needs_input && !working {
                return None;
            }

            let mut subagents = 0usize;
            let mut caption = None;
            for other in sessions.iter() {
                if let Some(tag) = &other.subagent {
                    if tag.parent_session_id == s.session_id {
                        if subagents == 0 {
                            caption = best_caption(
                                tag.status_label.as_deref(),
                                tag.narration.as_deref(),
                                &tag.description,
                            );
                        }
                        subagents += 1;
                    }
                }
            }

            Some(TickerItem {
                project: s.project.clone(),
                activity: s.activity.label(),
                needs_input,
                working,
                subagents,
                caption,
            })
        })
        .collect();

    compose(&items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working(project: &str, activity: &str) -> TickerItem {
        TickerItem {
            project: project.into(),
            activity: activity.into(),
            needs_input: false,
            working: true,
            subagents: 0,
            caption: None,
        }
    }

    fn needs_input(project: &str) -> TickerItem {
        TickerItem {
            project: project.into(),
            activity: "asking".into(),
            needs_input: true,
            working: false,
            subagents: 0,
            caption: None,
        }
    }

    #[test]
    fn idle_fallback_is_exact() {
        assert_eq!(compose(&[]), IDLE_TEXT);
    }

    #[test]
    fn a_working_session_renders_project_and_activity() {
        let items = [working("mewxi", "writing")];
        assert_eq!(compose(&items), "mewxi » writing");
    }

    #[test]
    fn needs_input_segments_sort_before_working_ones() {
        // Slice order interleaves them; needs-input must still lead.
        let items = [
            working("alpha", "thinking"),
            needs_input("bravo"),
            working("charlie", "reading"),
            needs_input("delta"),
        ];
        let out = compose(&items);
        let bravo_pos = out.find("bravo").unwrap();
        let delta_pos = out.find("delta").unwrap();
        let alpha_pos = out.find("alpha").unwrap();
        let charlie_pos = out.find("charlie").unwrap();
        assert!(bravo_pos < alpha_pos && bravo_pos < charlie_pos);
        assert!(delta_pos < alpha_pos && delta_pos < charlie_pos);
        // Within-group slice order preserved.
        assert!(bravo_pos < delta_pos);
        assert!(alpha_pos < charlie_pos);
    }

    #[test]
    fn subagent_count_and_caption_render() {
        let mut item = working("mewxi", "delegating");
        item.subagents = 2;
        item.caption = Some("checking rendering code".into());
        let out = compose(&[item]);
        assert_eq!(
            out,
            "mewxi » delegating +2⚡ · checking rendering code"
        );
    }

    #[test]
    fn caption_truncates_at_40_chars_with_ellipsis() {
        let long = "x".repeat(41);
        let got = best_caption(None, None, &long).unwrap();
        assert_eq!(got.chars().count(), CAPTION_MAX_CHARS + 1); // 40 + '…'
        assert!(got.ends_with('…'));
        assert_eq!(&got[..got.len() - '…'.len_utf8()], "x".repeat(40).as_str());

        let exact = "y".repeat(CAPTION_MAX_CHARS);
        let got_exact = best_caption(None, None, &exact).unwrap();
        assert_eq!(got_exact, exact);
        assert!(!got_exact.ends_with('…'));
    }

    #[test]
    fn caption_precedence_status_then_narration_then_description() {
        assert_eq!(
            best_caption(Some("on Bash(cargo test)"), Some("narrating"), "desc"),
            Some("on Bash(cargo test)".to_string())
        );
        assert_eq!(
            best_caption(None, Some("narrating"), "desc"),
            Some("narrating".to_string())
        );
        assert_eq!(best_caption(None, None, "desc"), Some("desc".to_string()));

        // Blank/whitespace-only values fall through to the next source.
        assert_eq!(
            best_caption(Some("   "), Some("narrating"), "desc"),
            Some("narrating".to_string())
        );
        assert_eq!(
            best_caption(Some(""), Some(""), "desc"),
            Some("desc".to_string())
        );
        assert_eq!(best_caption(Some(" "), Some(""), "   "), None);
    }

    #[test]
    fn six_segment_cap_emits_more_count() {
        let items: Vec<TickerItem> =
            (0..9).map(|i| working(&format!("proj{i}"), "thinking")).collect();
        let out = compose(&items);
        let segment_count = out.split(" ░ ").count();
        assert_eq!(segment_count, MAX_SEGMENTS + 1); // 6 shown + "+3 more"
        assert!(out.ends_with("+3 more"));
        assert!(out.contains("proj0"));
        assert!(out.contains("proj5"));
        assert!(!out.contains("proj6"));
    }
}
