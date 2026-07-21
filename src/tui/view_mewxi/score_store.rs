//! Load/save for `scores.json`, the local high-score file backing view
//! 5's arcade mode (best/current score, combo, and streak seconds), plus a
//! capped history of completed arcade runs.
//!
//! Writes are atomic: the new content lands in a `.json.tmp` sibling
//! file first, then an `fs::rename` swaps it into place, so a reader
//! never observes a half-written file and a crash mid-write can't
//! corrupt the real one. Reads are equally defensive in the other
//! direction — a missing, unreadable, or corrupt file quietly falls
//! back to [`ScoresFile::default`] rather than erroring. Nothing in
//! this module ever panics into the render path; the worst case on a
//! save failure is a one-line best-effort entry in the debug log.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One completed (banked) arcade run, as stored in `scores.json`.
/// Timestamps are RFC3339 UTC; `ended_at` is empty when unknown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub score: u64,
    pub peak_combo: u64,
    pub peak_streak_secs: f64,
    pub ended_at: String,
}

/// JSON shape of scores.json. All keys required, exactly these names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoresFile {
    pub best_score: u64,
    pub best_combo: u64,
    pub current_score: u64,
    pub current_combo: u64,
    pub current_streak_secs: f64,
    pub updated_at: String, // RFC3339 UTC string
    /// Completed runs, newest first, capped by the caller at 50. Absent
    /// in pre-history files, hence `#[serde(default)]`.
    #[serde(default)]
    pub history: Vec<RunRecord>,
}

impl Default for ScoresFile {
    fn default() -> Self {
        ScoresFile {
            best_score: 0,
            best_combo: 0,
            current_score: 0,
            current_combo: 0,
            current_streak_secs: 0.0,
            updated_at: String::new(),
            history: Vec::new(),
        }
    }
}

/// `$XDG_STATE_HOME/mewxi/scores.json`, else `~/.local/state/mewxi/scores.json`.
pub fn scores_file_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("mewxi").join("scores.json"))
}

/// Read+parse `path`. Missing/corrupt/unreadable => `ScoresFile::default()`. Never panics.
pub fn load_from(path: &Path) -> ScoresFile {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => ScoresFile::default(),
    }
}

/// Atomically write `scores` VERBATIM to `path` (temp file in same dir + rename).
/// Creates parent dirs. Writes exactly what is given (so round-trip is exact — does NOT stamp updated_at).
pub fn save_to(path: &Path, scores: &ScoresFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(scores)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolve the real path and load; defaults if the path can't be resolved.
pub fn load() -> ScoresFile {
    match scores_file_path() {
        Some(path) => load_from(&path),
        None => ScoresFile::default(),
    }
}

/// Stamp `updated_at = Utc::now().to_rfc3339()`, then `save_to` the real path.
/// Errors are SWALLOWED (best-effort debug_log, never propagated/panicked).
pub fn save(scores: &ScoresFile) {
    let Some(path) = scores_file_path() else { return };
    let mut stamped = scores.clone();
    stamped.updated_at = chrono::Utc::now().to_rfc3339();
    if let Err(e) = save_to(&path, &stamped) {
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Tui,
            crate::debug_log::LogKind::FileWrite,
            &format!("scores.json write failed — {e}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scores.json");
        let scores = ScoresFile {
            best_score: 12_345,
            best_combo: 42,
            current_score: 6_789,
            current_combo: 7,
            current_streak_secs: 123.456,
            updated_at: "2026-07-21T00:00:00+00:00".to_string(),
            history: vec![RunRecord {
                score: 42_000,
                peak_combo: 9,
                peak_streak_secs: 321.5,
                ended_at: "2026-07-20T00:00:00+00:00".to_string(),
            }],
        };
        save_to(&path, &scores).expect("save_to should succeed");
        let loaded = load_from(&path);
        assert_eq!(loaded, scores);
    }

    #[test]
    fn old_file_without_history_key_loads_with_empty_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scores.json");
        let json = r#"{"best_score":5000,"best_combo":7,"current_score":100,"current_combo":2,"current_streak_secs":12.5,"updated_at":"2026-07-20T00:00:00+00:00"}"#;
        std::fs::write(&path, json).expect("write old-format file");
        let loaded = load_from(&path);
        assert_eq!(loaded.best_score, 5000);
        assert_eq!(loaded.best_combo, 7);
        assert_eq!(loaded.current_score, 100);
        assert_eq!(loaded.current_combo, 2);
        assert_eq!(loaded.current_streak_secs, 12.5);
        assert_eq!(loaded.updated_at, "2026-07-20T00:00:00+00:00");
        assert!(loaded.history.is_empty());
    }

    #[test]
    fn empty_history_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scores.json");
        let scores = ScoresFile {
            best_score: 1,
            best_combo: 1,
            current_score: 1,
            current_combo: 1,
            current_streak_secs: 1.0,
            updated_at: "2026-07-21T00:00:00+00:00".to_string(),
            history: Vec::new(),
        };
        save_to(&path, &scores).expect("save_to should succeed");
        let loaded = load_from(&path);
        assert_eq!(loaded, scores);
        assert!(loaded.history.is_empty());
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scores.json");
        std::fs::write(&path, b"{ not valid json ]").expect("write corrupt file");
        let loaded = load_from(&path);
        assert_eq!(loaded, ScoresFile::default());
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        let loaded = load_from(&path);
        assert_eq!(loaded, ScoresFile::default());
    }

    #[test]
    fn save_to_creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deeper").join("scores.json");
        let scores = ScoresFile::default();
        save_to(&path, &scores).expect("save_to should create parent dirs and succeed");
        assert!(path.exists());
    }
}
