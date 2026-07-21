//! Shared purple/neon "rave" color palette for view 5.
//!
//! All colors are `Color::Indexed` in the 256-color purple/pink range —
//! deliberately never truecolor/RGB, so the palette stays consistent across
//! terminals with only 256-color support. Every sibling module in
//! `view_mewxi` imports this as `super::palette::…`.

use crate::live_session::Activity;
use ratatui::style::Color;

/// Dark purple — the dimmest step on the scale.
pub const P_DIM: Color = Color::Indexed(54);
/// Muted purple.
pub const P_LOW: Color = Color::Indexed(97);
/// Medium purple.
pub const P_MID: Color = Color::Indexed(135);
/// Bright purple.
pub const P_HIGH: Color = Color::Indexed(171);
/// Hot pink-purple.
pub const P_HOT: Color = Color::Indexed(207);
/// Pink.
pub const P_PINK: Color = Color::Indexed(213);
/// Neon lavender/white-pink highlight — the hottest step on the scale.
pub const P_NEON: Color = Color::Indexed(219);
/// Light lavender body text.
pub const P_TEXT: Color = Color::Indexed(183);
/// Label lavender.
pub const P_LABEL: Color = Color::Indexed(141);
/// Dark gauge/backdrop fill.
pub const P_BG: Color = Color::Indexed(53);

/// Block glyph ramp used by the visualizer, low → tall.
pub const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Gauge/utilisation fill colour, hotter as pct climbs.
pub fn purple_gauge(pct: f64) -> Color {
    if pct >= 90.0 {
        P_HOT
    } else if pct >= 70.0 {
        P_HIGH
    } else if pct >= 40.0 {
        P_MID
    } else {
        P_LOW
    }
}

/// Colour for a session [`Activity`] in the purple scale.
pub fn activity_color(a: &Activity) -> Color {
    match a {
        Activity::Waiting => P_DIM,
        Activity::Starting => P_MID,
        Activity::Thinking => P_MID,
        Activity::Compacting => P_MID,
        Activity::Writing => P_HOT,
        Activity::Editing => P_HOT,
        Activity::Running => P_HIGH,
        Activity::Delegating => P_HIGH,
        Activity::Reading => P_LOW,
        Activity::Searching => P_LOW,
        Activity::Fetching => P_LOW,
        Activity::Asking => P_HOT,
        Activity::Awaiting => P_HOT,
        Activity::Tool(_) => P_TEXT,
    }
}

/// Heat colour for a normalized value `frac` in `0.0..=1.0`: low = deep
/// purple, high = hot pink/neon. `frac` is clamped to `0.0..=1.0`.
pub fn heat_color(frac: f64) -> Color {
    let frac = frac.clamp(0.0, 1.0);
    if frac >= 0.95 {
        P_NEON
    } else if frac >= 0.8 {
        P_PINK
    } else if frac >= 0.6 {
        P_HOT
    } else if frac >= 0.4 {
        P_HIGH
    } else if frac >= 0.2 {
        P_MID
    } else if frac > 0.0 {
        P_LOW
    } else {
        P_DIM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purple_gauge_boundaries() {
        assert_eq!(purple_gauge(0.0), P_LOW);
        assert_eq!(purple_gauge(40.0), P_MID);
        assert_eq!(purple_gauge(70.0), P_HIGH);
        assert_eq!(purple_gauge(90.0), P_HOT);
        assert_eq!(purple_gauge(95.0), P_HOT);
        assert_eq!(purple_gauge(39.999), P_LOW);
        assert_eq!(purple_gauge(69.999), P_MID);
        assert_eq!(purple_gauge(89.999), P_HIGH);
    }

    #[test]
    fn heat_color_endpoints_and_clamping() {
        // Low end.
        let low = heat_color(0.0);
        assert!([P_DIM, P_LOW, P_MID, P_HIGH, P_PINK, P_NEON, P_HOT].contains(&low));

        // High end.
        let high = heat_color(1.0);
        assert!([P_DIM, P_LOW, P_MID, P_HIGH, P_PINK, P_NEON, P_HOT].contains(&high));

        // Out-of-range values must not panic and must clamp sensibly.
        assert_eq!(heat_color(-5.0), heat_color(0.0));
        assert_eq!(heat_color(5.0), heat_color(1.0));
    }

    #[test]
    fn activity_color_covers_all_variants() {
        // Just make sure every variant maps to something without panicking.
        let variants = [
            Activity::Waiting,
            Activity::Starting,
            Activity::Thinking,
            Activity::Writing,
            Activity::Reading,
            Activity::Editing,
            Activity::Searching,
            Activity::Fetching,
            Activity::Running,
            Activity::Delegating,
            Activity::Asking,
            Activity::Awaiting,
            Activity::Compacting,
            Activity::Tool("bash".to_string()),
        ];
        for v in variants.iter() {
            let _ = activity_color(v);
        }
        assert_eq!(activity_color(&Activity::Waiting), P_DIM);
        assert_eq!(activity_color(&Activity::Tool("x".into())), P_TEXT);
    }
}
