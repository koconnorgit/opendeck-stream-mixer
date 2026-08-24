use openaction::{Action, Instance, visible_instances};
use tux_icons::icon_fetcher::IconFetcher;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::gfx::TRANSPARENT_ICON;
use crate::mixer::{self, MixerChannel};
use crate::mixer::ENCODER_TO_CHANNEL_MAP;
use crate::plugin::{COLUMN_TO_CHANNEL_MAP, VolumeControllerAction};

const MAX_TITLE_CHARS_BEFORE_TRUNCATION: usize = 8;

/// The generic fallback icon, embedded in the binary so it is always available
/// regardless of the process working directory. See `resolve_icon_source`.
const DEFAULT_ICON: &[u8] = include_bytes!("../img/wave-sound.png");

// Global flag to track if system mixer should be shown
static SHOW_SYSTEM_MIXER: AtomicBool = AtomicBool::new(false);

pub struct ButtonPressControl {
    pub action_id: Option<String>,
    time_ms: Option<u128>,
}

impl ButtonPressControl {
    pub fn new() -> Self {
        ButtonPressControl {
            action_id: None,
            time_ms: None,
        }
    }

    pub fn set_press_time(&mut self, action_id: String) {
        self.action_id = Some(action_id);
        self.time_ms = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        );
    }

    pub fn get_release_time(&mut self) -> Option<u128> {
        self.action_id.as_ref()?;
        self.action_id = None;

        if let Some(press_time) = self.time_ms.take() {
            let release_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            let duration = release_time - press_time;
            return Some(duration);
        }
        None
    }
}

// Public getter for the global show_system_mixer flag
pub fn should_show_system_mixer() -> bool {
    SHOW_SYSTEM_MIXER.load(Ordering::Relaxed)
}

// Set the global flag
pub fn set_show_system_mixer(value: bool) {
    SHOW_SYSTEM_MIXER.store(value, Ordering::Relaxed);
}

pub async fn get_device_row_count() -> Option<u8> {
    let instances = visible_instances(VolumeControllerAction::UUID).await;
    if instances.is_empty() {
        return None;
    }

    let max_row = instances
        .iter()
        .filter_map(|i| i.coordinates.as_ref())
        .map(|coords| coords.row)
        .max()?;

    Some(max_row + 1)
}

pub async fn update_stream_deck_buttons() {
    let column_map = COLUMN_TO_CHANNEL_MAP.lock().await;
    let encoder_map = ENCODER_TO_CHANNEL_MAP.lock().await;
    let mut channels = mixer::MIXER_CHANNELS.lock().await;

    for instance in visible_instances(VolumeControllerAction::UUID).await {
        if instance.controller == "Encoder" {
            let dial_pos = instance
                .coordinates
                .as_ref()
                .map(|c| c.column)
                .unwrap_or(0);

            let Some(&channel_index) = encoder_map.get(&dial_pos) else {
                cleanup_encoder_dial(&instance).await;
                continue;
            };

            let Some(channel) = channels.get_mut(&channel_index) else {
                cleanup_encoder_dial(&instance).await;
                continue;
            };

            channel.dial_id = Some(instance.instance_id.clone());
            // Skip LCD re-render for scrolling channels — the scroll timer handles those.
            // Re-rendering here would flash a static frame and cause visible flicker.
            if !crate::scroll::is_lcd_scrolling(channel_index).await {
                update_encoder_dial(channel, &instance).await;
            }
        } else {
            let Some(coords) = instance.coordinates else {
                continue;
            };

            let Some(&channel_index) = column_map.get(&coords.column) else {
                continue;
            };

            let Some(channel) = channels.get_mut(&channel_index) else {
                cleanup_sd_column(&instance).await;
                continue;
            };

            match coords.row {
                0 => channel.header_id = Some(instance.instance_id.clone()),
                1 => channel.upper_vol_btn_id = Some(instance.instance_id.clone()),
                2 => channel.lower_vol_btn_id = Some(instance.instance_id.clone()),
                _ => {}
            }

            update_sd_column(channel, &instance).await;
        }
    }
}

pub async fn update_encoder_dial(channel: &MixerChannel, instance: &Instance) {
    let icon = if channel.mute {
        &channel.icon_uri_mute
    } else {
        &channel.icon_uri
    };
    let display_name = channel.display_label();
    match crate::gfx::get_encoder_lcd_data_uri(icon, &display_name, channel.vol_percent, channel.mute, 0.0) {
        Ok(uri) => {
            let _ = instance.set_image(Some(uri), None).await;
        }
        Err(e) => eprintln!("Failed to render encoder LCD image: {e}"),
    }
    // The app name is now rendered into the LCD image itself by gfx.rs, so clear
    // the action title. OpenDeck's frontend composites the title as an overlay on
    // top of the image before sending it to the device; leaving a title set would
    // cause the name to appear twice (once drawn by us, once overlaid).
    let _ = instance.set_title(Some(""), None).await;
}

pub async fn cleanup_encoder_dial(instance: &Instance) {
    let _ = instance.set_title(Some(""), None).await;
    let _ = instance.set_image(Some(TRANSPARENT_ICON.as_str()), None).await;
}

pub async fn update_header(instance: &Instance, channel: &MixerChannel) {
    let icon_uri = if channel.mute {
        channel.icon_uri_mute.clone()
    } else {
        channel.icon_uri.clone()
    };

    let _ = instance.set_image(Some(icon_uri), None).await;

    // Set title based on priority: multi-sink app > uses default icon > no title
    if channel.is_multi_sink_app {
        let _ = instance
            .set_title(
                channel.sink_name.as_ref().map(|name| {
                    if name.len() > MAX_TITLE_CHARS_BEFORE_TRUNCATION {
                        format!(
                            "{}...",
                            name.chars()
                                .take(MAX_TITLE_CHARS_BEFORE_TRUNCATION)
                                .collect::<String>()
                        )
                    } else {
                        name.clone()
                    }
                }),
                None,
            )
            .await;
    } else if channel.uses_default_icon {
        let _ = instance
            .set_title(Some(channel.app_name.clone()), None)
            .await;
    } else {
        let _ = instance.set_title(Some(""), None).await;
    }
}

/// Minimum largest-dimension for a window's `_NET_WM_ICON` to be preferred over
/// a themed icon. Above this we trust the real window pixels (e.g. Steam's 128px
/// icon beats its monochrome tray theme icon); below it we prefer the (usually
/// higher-res) themed icon and keep the small window icon only as a fallback.
const HIGH_RES_WINDOW_ICON: u32 = 64;

/// Resolve the best icon source for a stream as `(bytes, mime, uses_default)`.
///
/// Precedence: MPRIS media art → high-res window icon → themed icon (by the best
/// available keys) → any window icon → bundled default. The themed-icon lookup
/// is only performed when the earlier, cheaper sources don't apply.
fn resolve_icon_source(
    icon_name: Option<String>,
    fallback_icon_name: String,
    mpris_art_data: Option<&[u8]>,
    wm_class: Option<&str>,
    window_icon: Option<&crate::window_icons::WindowIcon>,
) -> (Vec<u8>, String, bool) {
    // 1. MPRIS media art (browsers).
    if let Some(data) = mpris_art_data {
        return (data.to_vec(), "image/png".to_string(), false);
    }

    // 2. A high-res real window icon beats a possibly-poor themed icon.
    if let Some(wi) = window_icon {
        if wi.max_dim >= HIGH_RES_WINDOW_ICON {
            return (wi.png.clone(), "image/png".to_string(), false);
        }
    }

    // 3. Themed icon, trying the cleanest keys first.
    let mut keys: Vec<String> = Vec::new();
    if let Some(name) = icon_name {
        keys.push(name);
    }
    if let Some(wc) = wm_class {
        keys.push(wc.to_string());
        // Themed files often use a hyphenated/condensed form of a spaced app-id.
        if wc.contains(' ') {
            keys.push(wc.replace(' ', "-"));
            keys.push(wc.replace(' ', ""));
        }
    }
    keys.push(fallback_icon_name);

    let fetcher = IconFetcher::new();
    if let Some(path) = keys.iter().find_map(|k| fetcher.get_icon_path(k.clone())) {
        if let Ok(bytes) = std::fs::read(&path) {
            let mime = match path.extension().and_then(|e| e.to_str()) {
                Some("svg") => "image/svg+xml",
                Some("xpm") => "image/x-xpm",
                _ => "image/png",
            };
            return (bytes, mime.to_string(), false);
        }
    }

    // 4. Any window icon (small) before falling back to the generic default.
    if let Some(wi) = window_icon {
        return (wi.png.clone(), "image/png".to_string(), false);
    }

    // 5. Bundled default. Embedded at compile time so the fallback icon can never
    // silently become a blank image — reading it from a relative path would depend
    // on the plugin's working directory being the sdPlugin dir, an undocumented
    // assumption that, if it ever changed, would break every default icon at once.
    (DEFAULT_ICON.to_vec(), "image/png".to_string(), true)
}

/// Get application icon as base64 data URIs.
/// Resolves the best available source (MPRIS art, window icon, themed icon, or
/// the bundled default) and returns (normal_uri, muted_uri, uses_default_icon).
/// Wall-clock stamp for identity-trace log lines (`HH:MM:SS.mmm`, UTC), so
/// the ordering of departures, session bumps and recoveries is recoverable
/// from the plugin log when diagnosing a mis-assignment.
pub fn log_ts(t: SystemTime) -> String {
    let ms = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        (s / 3600) % 24,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

pub fn get_app_icon_uri(
    icon_name: Option<String>,
    fallback_icon_name: String,
    mpris_art_data: Option<&[u8]>,
    wm_class: Option<&str>,
    window_icon: Option<&crate::window_icons::WindowIcon>,
) -> (String, String, bool) {
    use base64::{Engine as _, engine::general_purpose};

    let (image_data, mime_type, uses_default_icon) = resolve_icon_source(
        icon_name,
        fallback_icon_name,
        mpris_art_data,
        wm_class,
        window_icon,
    );

    let base64_normal = general_purpose::STANDARD.encode(&image_data);
    let normal_uri = format!("data:{};base64,{}", mime_type, base64_normal);

    // grayscale on mute
    let muted_uri = if mime_type == "image/svg+xml" {
        if let Ok(svg_string) = String::from_utf8(image_data.clone()) {
            let grayscale_svg = add_grayscale_filter_to_svg(svg_string);
            let base64_gray = general_purpose::STANDARD.encode(grayscale_svg.as_bytes());
            format!("data:image/svg+xml;base64,{}", base64_gray)
        } else {
            normal_uri.clone()
        }
    } else if let Ok(img) = image::load_from_memory(&image_data) {
        let gray_img = image::DynamicImage::ImageLuma8(img.to_luma8());
        let mut buffer = std::io::Cursor::new(Vec::new());
        if gray_img
            .write_to(&mut buffer, image::ImageFormat::Png)
            .is_ok()
        {
            let gray_data = buffer.into_inner();
            let base64_gray = general_purpose::STANDARD.encode(&gray_data);
            format!("data:image/png;base64,{}", base64_gray)
        } else {
            normal_uri.clone()
        }
    } else {
        normal_uri.clone()
    };

    (normal_uri, muted_uri, uses_default_icon)
}

pub async fn cleanup_sd_column(instance: &Instance) {
    let _ = instance.set_title(Some(""), None).await;
    let _ = instance
        .set_image(Some(TRANSPARENT_ICON.as_str()), None)
        .await;
}

/// Add a grayscale CSS filter to an SVG
fn add_grayscale_filter_to_svg(svg: String) -> String {
    // Check if the SVG already has a <defs> section
    if let Some(svg_tag_end) = svg.find('>') {
        let before_close = &svg[..svg_tag_end + 1];
        let after_open = &svg[svg_tag_end + 1..];

        // Simply reduce opacity instead of using filters (avoids blur)
        if before_close.contains("opacity=") {
            svg
        } else {
            let svg_tag_modified = before_close.replace("<svg", r#"<svg opacity="0.4""#);
            format!("{}{}", svg_tag_modified, after_open)
        }
    } else {
        svg
    }
}

async fn update_sd_column(channel: &MixerChannel, instance: &Instance) {
    let Some(coords) = instance.coordinates else {
        return;
    };

    match coords.row {
        0 => {
            update_header(instance, channel).await;
        }
        1 | 2 => {
            // Update volume buttons with bar graphics
            if let Ok((upper_img, lower_img)) =
                crate::gfx::get_volume_bar_data_uri_split(channel.vol_percent)
            {
                if coords.row == 1 {
                    let _ = instance.set_image(Some(upper_img), None).await;
                } else {
                    let _ = instance.set_image(Some(lower_img), None).await;
                }
            }
        }
        _ => {}
    }
}
