//! Stream-title utilities shared by identity matching and display.
//!
//! Historical note: until phase 3 of docs/mpris-subscription-design.md this
//! module also scavenged Firefox's `firefox-mpris/` art files off disk and
//! correlated them to streams by write-time — deleted because Firefox keeps
//! only ONE art file at a time (the previous one is removed on every session
//! change), so the directory structurally cannot attribute art during churn.
//! Art now comes from `media_session` (bytes captured at signal time) and the
//! deterministic fetchers in `art_resolve` / `kick`.

use std::time::Duration;

/// Check if a name is a generic placeholder that shouldn't be displayed.
pub fn is_generic_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "audiostream"
        || lower == "audio stream"
        || lower == "audio-src"
        || lower == "app_stream"
        || lower == "playback"
}

/// Trailing boilerplate that browsers append to the page <title>. Stripped
/// (case-insensitively) so identity matching compares bare titles.
const TITLE_SUFFIXES: &[&str] = &[
    " - Watch Live on Kick",
    " - Kick",
    " - Twitch",
    " - YouTube",
];

/// Strip known site boilerplate suffixes from a page title.
/// "PaymoneyWubby Stream - Watch Live on Kick" -> "PaymoneyWubby Stream"
pub(crate) fn clean_stream_title(title: &str) -> String {
    let trimmed = title.trim();
    for suffix in TITLE_SUFFIXES {
        let Some(idx) = trimmed.len().checked_sub(suffix.len()) else {
            continue;
        };
        // A multi-byte char straddling the split point means the ASCII suffix
        // can't be there; slicing would panic (titles carry emoji), so check
        // the boundary.
        if trimmed.is_char_boundary(idx) && trimmed[idx..].eq_ignore_ascii_case(suffix) {
            return trimmed[..idx].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Schedule a delayed full refresh.
/// Waits 2 seconds for PulseAudio metadata and media-session state to settle,
/// then re-queries everything so text and icons both update.
pub fn schedule_delayed_refresh() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        crate::audio::pulse::pulse_monitor::refresh_guarded("delayed").await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_kick_suffix() {
        assert_eq!(
            clean_stream_title("PaymoneyWubby Stream - Watch Live on Kick"),
            "PaymoneyWubby Stream"
        );
    }

    #[test]
    fn strips_case_insensitively_and_trims() {
        assert_eq!(clean_stream_title("Cool Channel - youtube"), "Cool Channel");
        assert_eq!(clean_stream_title("  Spaced Out - Twitch  "), "Spaced Out");
    }

    #[test]
    fn leaves_unrecognized_titles_untouched() {
        assert_eq!(clean_stream_title("Just A Title"), "Just A Title");
    }

    #[test]
    fn emoji_title_with_mid_char_split_point_does_not_panic() {
        // Regression: same unchecked byte-slice bug as kick.rs strip_suffix_ci —
        // len - suffix.len() landing inside a multi-byte char panicked.
        assert_eq!(clean_stream_title("🎵🎵🎵🎵"), "🎵🎵🎵🎵");
        assert_eq!(clean_stream_title("🎵 Song - YouTube"), "🎵 Song");
    }
}
