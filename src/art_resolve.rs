//! Deterministic, identity-keyed art fetching — phase 2 of
//! `docs/mpris-subscription-design.md`.
//!
//! When a channel's identity is known but the browser never published art for
//! it (or the session predates the plugin), the art can be fetched from the
//! site directly: YouTube video thumbnails by video id, Twitch streamer
//! avatars by login. Art must always be the avatar or content thumbnail —
//! NEVER a live-stream screenshot (user decision 2026-07-04; the preview-CDN
//! fallback was shipped and rejected on sight). The Twitch avatar URL is
//! resolved through decapi.me — a third-party, no-auth, plain-text resolver —
//! because Twitch's own avatar API (Helix) needs a registered key. Kick is
//! NOT handled here; `kick.rs` already owns that fetch, keyed by slug.
//!
//! Same cache/backoff state machine and curl transport as `kick.rs` (see its
//! module docs for why curl-the-binary). Images are center-cropped square so
//! 16:9 thumbnails don't letterbox on the LCD tile.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long to sit on a failed fetch before retrying that identity (offline
/// Twitch channels 404 their preview; don't hammer the CDN).
const FAILURE_BACKOFF: Duration = Duration::from_secs(300);

pub enum ArtState {
    Ready(Arc<Vec<u8>>),
    /// Fetch in flight; a refresh is nudged when it lands.
    Pending,
    /// No fetcher for this identity, or recently failed.
    Unavailable,
}

enum Entry {
    Fetching,
    Ready(Arc<Vec<u8>>),
    Failed(Instant),
}

static CACHE: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whether an identity key has a deterministic fetcher here.
fn has_fetcher(key: &str) -> bool {
    key.starts_with("yt:") || key.starts_with("ttv:")
}

/// decapi answers with the avatar URL as plain text (or an error sentence,
/// e.g. "User not found"). Only a clean single-token https URL is trusted.
fn parse_decapi_avatar(resp: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(resp).ok()?.trim();
    (s.starts_with("https://") && !s.contains(char::is_whitespace)).then(|| s.to_string())
}

/// Blocking: resolve an identity to raw image bytes.
fn fetch_art_bytes(key: &str) -> Option<Vec<u8>> {
    if let Some(id) = key.strip_prefix("yt:") {
        // The video thumbnail (what the user sees in search/suggestions).
        return crate::kick::curl_get(&format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"));
    }
    if let Some(login) = key.strip_prefix("ttv:") {
        // The streamer avatar — two hops: decapi resolves login → avatar URL.
        let resp = crate::kick::curl_get(&format!("https://decapi.me/twitch/avatar/{login}"))?;
        let url = parse_decapi_avatar(&resp)?;
        return crate::kick::curl_get(&url);
    }
    None
}

/// Look up (or start fetching) art for an identity key. Non-blocking: a cold
/// key kicks off a background fetch and reports `Pending`; the completed fetch
/// nudges a refresh so the mixer re-renders with the result.
pub fn resolve(key: &str) -> ArtState {
    if !has_fetcher(key) {
        return ArtState::Unavailable;
    }
    let mut cache = CACHE.lock().unwrap();
    match cache.get(key) {
        Some(Entry::Ready(bytes)) => ArtState::Ready(bytes.clone()),
        Some(Entry::Fetching) => ArtState::Pending,
        Some(Entry::Failed(at)) if at.elapsed() < FAILURE_BACKOFF => ArtState::Unavailable,
        _ => {
            cache.insert(key.to_string(), Entry::Fetching);
            spawn_fetch(key.to_string());
            ArtState::Pending
        }
    }
}

/// Fetch one identity's art off-thread, cache the result, and queue a
/// (debounced, panic-guarded) refresh so the mixer picks it up.
fn spawn_fetch(key: String) {
    tokio::spawn(async move {
        let fetch_key = key.clone();
        let fetched = tokio::task::spawn_blocking(move || fetch_square_png(&fetch_key))
            .await
            .unwrap_or(None);
        {
            let mut cache = CACHE.lock().unwrap();
            let entry = match &fetched {
                Some(bytes) => Entry::Ready(Arc::new(bytes.clone())),
                None => Entry::Failed(Instant::now()),
            };
            cache.insert(key.clone(), entry);
        }
        println!(
            "ART-RESOLVE {key}: {}",
            fetched.as_ref().map(|b| format!("{}KB", b.len() / 1024)).unwrap_or("failed".into())
        );
        crate::audio::pulse::pulse_monitor::request_refresh();
    });
}

/// Blocking: fetch an identity's image, center-crop it square, re-encode as
/// PNG (the icon pipeline treats art as PNG, incl. the grayscale-on-mute step).
fn fetch_square_png(key: &str) -> Option<Vec<u8>> {
    let bytes = fetch_art_bytes(key)?;
    let img = image::load_from_memory(&bytes).ok()?;
    square_png(img)
}

fn square_png(img: image::DynamicImage) -> Option<Vec<u8>> {
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let side = w.min(h);
    let img = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetchers_by_identity() {
        assert!(has_fetcher("yt:eAz9dcpAz-Q"));
        assert!(has_fetcher("ttv:nmplol"));
        // Kick is owned by kick.rs; generic web has no fetcher.
        assert!(!has_fetcher("kick:xqc"));
        assert!(!has_fetcher("web:reddit.com:r/livestreamfail"));
    }

    #[test]
    fn decapi_avatar_url_parsed_and_errors_rejected() {
        assert_eq!(
            parse_decapi_avatar(
                b"https://static-cdn.jtvnw.net/jtv_user_pictures/abc-profile_image-300x300.png\n"
            )
            .as_deref(),
            Some("https://static-cdn.jtvnw.net/jtv_user_pictures/abc-profile_image-300x300.png")
        );
        assert_eq!(parse_decapi_avatar(b"User not found"), None);
        assert_eq!(parse_decapi_avatar(b""), None);
    }

    // Live check against decapi + Twitch's CDN (needs network + curl). Run with:
    //   cargo test -- --ignored --nocapture live_fetch_twitch_avatar
    #[test]
    #[ignore]
    fn live_fetch_twitch_avatar() {
        let png = fetch_square_png("ttv:nmplol").expect("expected an avatar");
        let img = image::load_from_memory(&png).expect("decodes as PNG");
        use image::GenericImageView;
        let (w, h) = img.dimensions();
        println!("avatar: {} bytes, {}x{}", png.len(), w, h);
        assert_eq!(w, h, "avatar is square");
    }

    #[test]
    fn square_crop_takes_center_of_wide_image() {
        use image::GenericImageView;
        // 32x18 (16:9-ish) → 18x18 centered.
        let wide = image::DynamicImage::new_rgba8(32, 18);
        let png = square_png(wide).expect("crop+encode succeeds");
        let out = image::load_from_memory(&png).unwrap();
        assert_eq!(out.dimensions(), (18, 18));
    }
}
