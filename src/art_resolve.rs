//! Deterministic, identity-keyed art fetching — phase 2 of
//! `docs/mpris-subscription-design.md`.
//!
//! When a channel's identity is known but the browser never published art for
//! it (or the session predates the plugin), the art can be fetched from the
//! site directly: YouTube thumbnails by video id, Twitch live previews by
//! login (decided 2026-07-04: session art primary, preview CDN fallback —
//! both URLs need no auth). Kick is NOT handled here; `kick.rs` already owns
//! that fetch, keyed by slug.
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

/// The deterministic source URL for an identity key, if one exists.
fn fetch_url_for(key: &str) -> Option<String> {
    if let Some(id) = key.strip_prefix("yt:") {
        return Some(format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"));
    }
    if let Some(login) = key.strip_prefix("ttv:") {
        return Some(format!(
            "https://static-cdn.jtvnw.net/previews-ttv/live_user_{login}-320x180.jpg"
        ));
    }
    None
}

/// Look up (or start fetching) art for an identity key. Non-blocking: a cold
/// key kicks off a background fetch and reports `Pending`; the completed fetch
/// nudges a refresh so the mixer re-renders with the result.
pub fn resolve(key: &str) -> ArtState {
    let Some(url) = fetch_url_for(key) else {
        return ArtState::Unavailable;
    };
    let mut cache = CACHE.lock().unwrap();
    match cache.get(key) {
        Some(Entry::Ready(bytes)) => ArtState::Ready(bytes.clone()),
        Some(Entry::Fetching) => ArtState::Pending,
        Some(Entry::Failed(at)) if at.elapsed() < FAILURE_BACKOFF => ArtState::Unavailable,
        _ => {
            cache.insert(key.to_string(), Entry::Fetching);
            spawn_fetch(key.to_string(), url);
            ArtState::Pending
        }
    }
}

/// Fetch one identity's art off-thread, cache the result, and queue a
/// (debounced, panic-guarded) refresh so the mixer picks it up.
fn spawn_fetch(key: String, url: String) {
    tokio::spawn(async move {
        let fetched = tokio::task::spawn_blocking(move || fetch_square_png(&url))
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

/// Blocking: GET an image, center-crop it square, re-encode as PNG (the icon
/// pipeline treats art as PNG, incl. the grayscale-on-mute step).
fn fetch_square_png(url: &str) -> Option<Vec<u8>> {
    let bytes = crate::kick::curl_get(url)?;
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
    fn fetch_urls_by_identity() {
        assert_eq!(
            fetch_url_for("yt:eAz9dcpAz-Q").as_deref(),
            Some("https://i.ytimg.com/vi/eAz9dcpAz-Q/mqdefault.jpg")
        );
        assert_eq!(
            fetch_url_for("ttv:nmplol").as_deref(),
            Some("https://static-cdn.jtvnw.net/previews-ttv/live_user_nmplol-320x180.jpg")
        );
        // Kick is owned by kick.rs; generic web has no fetcher.
        assert_eq!(fetch_url_for("kick:xqc"), None);
        assert_eq!(fetch_url_for("web:reddit.com:r/livestreamfail"), None);
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
