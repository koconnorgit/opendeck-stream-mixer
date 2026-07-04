//! Kick.com edge case.
//!
//! Twitch/YouTube publish W3C Media Session metadata, so Firefox writes a media
//! art file that the MPRIS machinery (see [`crate::mpris`]) claims and shows as
//! the channel icon. Kick.com doesn't, so no art file is ever written and the
//! channel falls back to the generic Firefox icon — even though the channel has
//! a perfectly good streamer avatar exposed through Kick's public API.
//!
//! This module recovers the channel *slug* from the browser tab title (carried in
//! PulseAudio `media.name`), fetches the avatar from `kick.com/api/v2/channels`,
//! re-encodes it to PNG, and hands it to the mixer in the same slot MPRIS art
//! would fill. Results are cached by slug; a cold lookup kicks off a background
//! fetch that re-renders the mixer once the avatar lands.
//!
//! Network goes through the `curl` binary on purpose: Kick sits behind Cloudflare
//! which fingerprints the TLS stack and 403s pure-Rust clients (rustls), while a
//! browser-User-Agent request from curl passes. Missing/old curl, an offline box,
//! a renamed channel — any failure degrades silently to the default icon.

use std::collections::HashMap;
use std::io::Cursor;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Browser-like UA so Cloudflare lets the request through.
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Per-request curl timeout (seconds, as the string curl wants).
const FETCH_TIMEOUT_SECS: &str = "15";
/// How long to sit on a failed fetch before trying that slug again, so a bad
/// slug or an outage doesn't spawn a curl on every PulseAudio event.
const FAILURE_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Clone)]
enum AvatarState {
    /// A fetch task is in flight; don't start another for this slug.
    Fetching,
    /// Avatar fetched and re-encoded to PNG.
    Ready(Vec<u8>),
    /// Last fetch failed at this instant; retry once `FAILURE_BACKOFF` elapses.
    Failed(Instant),
}

static AVATARS: LazyLock<Mutex<HashMap<String, AvatarState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Case-insensitive suffix strip. Returns the prefix with `suffix` removed.
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let idx = s.len().checked_sub(suffix.len())?;
    // A multi-byte char straddling the split point means the ASCII suffix can't
    // be there; slicing would panic (titles carry emoji), so check the boundary.
    if s.is_char_boundary(idx) && s[idx..].eq_ignore_ascii_case(suffix) {
        Some(&s[..idx])
    } else {
        None
    }
}

/// Recover a Kick channel *handle* (original case) from a browser tab title
/// (PulseAudio `media.name`). `"PaymoneyWubby Stream - Watch Live on Kick"` ->
/// `"PaymoneyWubby"`.
///
/// Kick titles look like `"<handle> Stream - Watch Live on Kick"`. A Kick username
/// is a single `[A-Za-z0-9_]` token, so once the boilerplate is stripped the
/// remainder must be one such token — otherwise the title isn't something we can
/// turn into a handle and we return `None` rather than guess.
fn kick_handle(media_name: &str) -> Option<&str> {
    let name = media_name.trim();
    let body = strip_suffix_ci(name, " - Watch Live on Kick")
        .or_else(|| strip_suffix_ci(name, " - Kick"))?;
    let handle = strip_suffix_ci(body.trim(), " Stream").unwrap_or(body).trim();
    if !handle.is_empty() && handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(handle)
    } else {
        None
    }
}

/// Lowercased handle — the slug used for Kick's API and as the avatar cache key.
pub fn kick_slug(media_name: &str) -> Option<String> {
    kick_handle(media_name).map(|h| h.to_lowercase())
}

/// The streamer name to show on the LCD, recovered from a Kick tab title in its
/// original case (`"PaymoneyWubby Stream - Watch Live on Kick"` -> `"PaymoneyWubby"`).
/// Returns the input unchanged when it isn't a recognizable Kick title, so
/// non-Kick stream names pass through untouched.
pub fn display_name(app_name: &str) -> String {
    kick_handle(app_name).map_or_else(|| app_name.to_string(), |h| h.to_string())
}

/// Look up (or start fetching) a Kick streamer's avatar by slug, mirroring
/// `art_resolve::resolve` (phase 3: the mixer asks per identity instead of an
/// enrichment pass tagging AppInfo). Non-blocking: a cold slug kicks off a
/// background fetch and reports `Pending`; the completed fetch queues a refresh.
pub fn resolve_avatar(slug: &str) -> crate::art_resolve::ArtState {
    use crate::art_resolve::ArtState;
    let mut cache = AVATARS.lock().unwrap();
    match cache.get(slug) {
        Some(AvatarState::Ready(bytes)) => ArtState::Ready(std::sync::Arc::new(bytes.clone())),
        Some(AvatarState::Fetching) => ArtState::Pending,
        Some(AvatarState::Failed(at)) if at.elapsed() < FAILURE_BACKOFF => ArtState::Unavailable,
        _ => {
            // Never seen, or the failure backoff has expired → fetch it.
            cache.insert(slug.to_string(), AvatarState::Fetching);
            spawn_fetch(slug.to_string());
            ArtState::Pending
        }
    }
}

/// Fetch one slug's avatar off-thread, cache the result, and trigger a refresh so
/// the mixer re-renders with the avatar (or drops the pending flag on failure).
fn spawn_fetch(slug: String) {
    tokio::spawn(async move {
        let fetched = {
            let slug = slug.clone();
            tokio::task::spawn_blocking(move || fetch_avatar(&slug))
                .await
                .unwrap_or(None)
        };

        {
            let mut cache = AVATARS.lock().unwrap();
            let state = match &fetched {
                Some(bytes) => AvatarState::Ready(bytes.clone()),
                None => AvatarState::Failed(Instant::now()),
            };
            cache.insert(slug, state);
        }

        crate::audio::pulse::pulse_monitor::request_refresh();
    });
}

/// Blocking: resolve a slug to PNG avatar bytes via Kick's API. `None` on any
/// failure (curl missing, Cloudflare block, unknown channel, decode error).
fn fetch_avatar(slug: &str) -> Option<Vec<u8>> {
    let api = format!("https://kick.com/api/v2/channels/{slug}");
    let json = curl_get(&api)?;
    let pic_url = extract_profile_pic(&String::from_utf8_lossy(&json))?;
    let img_bytes = curl_get(&pic_url)?;

    // Kick serves lossy WebP; normalize to PNG so the icon pipeline (which treats
    // art as PNG, incl. the grayscale-on-mute step) handles it like MPRIS art.
    let decoded = image::load_from_memory(&img_bytes).ok()?;
    let mut buf = Cursor::new(Vec::new());
    decoded.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// HTTP GET via the `curl` binary (see module docs for why not a Rust client).
/// `-f` makes curl fail (non-zero exit, empty stdout) on HTTP errors.
/// Shared with `art_resolve` for the YouTube/Twitch art fetchers.
pub(crate) fn curl_get(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", FETCH_TIMEOUT_SECS, "-A", UA, url])
        .output()
        .ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    }
}

/// Pull the `profile_pic` URL out of a Kick channel API body without a JSON
/// dependency. Returns `None` if the field is absent or null.
fn extract_profile_pic(body: &str) -> Option<String> {
    const KEY: &str = "\"profile_pic\":";
    let after = body[body.find(KEY)? + KEY.len()..].trim_start();
    let rest = after.strip_prefix('"')?; // `null` (or anything non-string) → bail
    let end = rest.find('"')?;
    let url = rest[..end].replace("\\/", "/");
    (!url.is_empty()).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_from_standard_kick_title() {
        assert_eq!(
            kick_slug("asmongold Stream - Watch Live on Kick").as_deref(),
            Some("asmongold")
        );
    }

    #[test]
    fn slug_lowercased() {
        assert_eq!(
            kick_slug("xQc Stream - Watch Live on Kick").as_deref(),
            Some("xqc")
        );
    }

    #[test]
    fn slug_without_stream_word() {
        assert_eq!(
            kick_slug("trainwreckstv - Watch Live on Kick").as_deref(),
            Some("trainwreckstv")
        );
    }

    #[test]
    fn slug_allows_underscores() {
        assert_eq!(
            kick_slug("some_streamer Stream - Watch Live on Kick").as_deref(),
            Some("some_streamer")
        );
    }

    #[test]
    fn display_name_trims_kick_boilerplate_keeping_case() {
        assert_eq!(
            crate::kick::display_name("PaymoneyWubby Stream - Watch Live on Kick"),
            "PaymoneyWubby"
        );
        assert_eq!(
            crate::kick::display_name("trainwreckstv - Watch Live on Kick"),
            "trainwreckstv"
        );
    }

    #[test]
    fn display_name_passes_non_kick_through_unchanged() {
        assert_eq!(crate::kick::display_name("Spotify"), "Spotify");
        assert_eq!(
            crate::kick::display_name("Some Song - YouTube"),
            "Some Song - YouTube"
        );
    }

    #[test]
    fn emoji_title_with_mid_char_split_point_does_not_panic() {
        // Regression: "🎵🎵🎵🎵" is 16 bytes and len - " - Kick".len() = 9 falls
        // inside the third emoji; the old byte-slice panicked on exactly this
        // (live incident 2026-07-04, killed the refresh loop).
        assert_eq!(kick_slug("🎵🎵🎵🎵"), None);
        assert_eq!(display_name("🎵🎵🎵🎵"), "🎵🎵🎵🎵");
        assert_eq!(kick_slug("Lofi Beats 🎵🎵 to relax to 🎵"), None);
    }

    #[test]
    fn non_kick_titles_are_none() {
        assert_eq!(kick_slug("PaymoneyWubby - Twitch"), None);
        assert_eq!(kick_slug("Some Song - YouTube"), None);
        assert_eq!(kick_slug("Just A Title"), None);
    }

    #[test]
    fn multiword_remainder_is_not_a_slug() {
        // After stripping boilerplate this isn't a single username token, so we
        // refuse rather than fetch a bogus slug.
        assert_eq!(kick_slug("Some Cool Channel - Watch Live on Kick"), None);
    }

    #[test]
    fn extract_profile_pic_handles_escaped_slashes() {
        let body = r#"{"id":1,"profile_pic":"https:\/\/files.kick.com\/a\/b.webp","x":2}"#;
        assert_eq!(
            extract_profile_pic(body).as_deref(),
            Some("https://files.kick.com/a/b.webp")
        );
    }

    #[test]
    fn extract_profile_pic_null_is_none() {
        assert_eq!(extract_profile_pic(r#"{"profile_pic":null}"#), None);
    }

    #[test]
    fn extract_profile_pic_absent_is_none() {
        assert_eq!(extract_profile_pic(r#"{"slug":"x"}"#), None);
    }

    // Live check against Kick's API + Cloudflare (needs network + curl). Run with:
    //   cargo test -- --ignored --nocapture live_fetch_avatar
    #[test]
    #[ignore]
    fn live_fetch_avatar() {
        let png = fetch_avatar("asmongold").expect("expected an avatar");
        let img = image::load_from_memory(&png).expect("avatar should decode as PNG");
        use image::GenericImageView;
        println!("fetched avatar: {} bytes, dims {:?}", png.len(), img.dimensions());
        assert!(png.len() > 1000);
    }
}
