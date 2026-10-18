use ab_glyph::{Font, FontArc, Glyph, PxScale, ScaleFont, point};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use image::{Rgba, RgbaImage};
use std::collections::HashMap;
use std::fmt;
use std::io::Cursor;
use std::sync::{LazyLock, Mutex, OnceLock};

/// Lazily-loaded sans-serif bold font. Tries a list of common Linux system paths;
/// returns None if none are readable, in which case text rendering is a no-op.
static TITLE_FONT: LazyLock<Option<FontArc>> = LazyLock::new(|| {
    let paths: &[&str] = &[
        "/usr/share/fonts/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
    ];
    for path in paths {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = FontArc::try_from_vec(bytes) {
                return Some(font);
            }
        }
    }
    None
});

pub fn measure_text_width(font: &FontArc, text: &str, scale: PxScale) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut width = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for c in text.chars() {
        let gid = font.glyph_id(c);
        if let Some(p) = prev {
            width += scaled.kern(p, gid);
        }
        width += scaled.h_advance(gid);
        prev = Some(gid);
    }
    width
}

fn fit_text(font: &FontArc, text: &str, scale: PxScale, max_w: f32) -> String {
    if measure_text_width(font, text, scale) <= max_w {
        return text.to_string();
    }
    let mut s = text.to_string();
    while measure_text_width(font, &s, scale) > max_w && !s.is_empty() {
        s.pop();
    }
    s
}

/// Return a reference to the loaded title font, if available.
pub fn title_font() -> Option<&'static FontArc> {
    TITLE_FONT.as_ref()
}

/// Draw text horizontally centered within the rectangle
/// (area_x, area_y, area_w, area_h), alpha-blended onto `img`.
/// No-op if the system font is unavailable or the text is empty.
fn draw_text_centered(img: &mut RgbaImage, text: &str, area_x: u32, area_y: u32, area_w: u32, size_px: f32, color: Rgba<u8>) {
    let Some(font) = TITLE_FONT.as_ref() else {
        return;
    };
    if text.is_empty() {
        return;
    }

    let scale = PxScale::from(size_px);
    let fitted = fit_text(font, text, scale, area_w as f32 - 4.0);
    if fitted.is_empty() {
        return;
    }

    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let width = measure_text_width(font, &fitted, scale);
    let x_start = area_x as f32 + (area_w as f32 - width) / 2.0;
    let y_baseline = area_y as f32 + ascent + 1.0;

    let mut x_cursor = x_start;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for c in fitted.chars() {
        let gid = font.glyph_id(c);
        if let Some(p) = prev {
            x_cursor += scaled.kern(p, gid);
        }
        let glyph: Glyph = gid.with_scale_and_position(scale, point(x_cursor, y_baseline));

        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 && (px as u32) < img.width() && (py as u32) < img.height() {
                    let bg = *img.get_pixel(px as u32, py as u32);
                    let a = coverage * (color[3] as f32 / 255.0);
                    let r = (color[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8;
                    let g = (color[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8;
                    let b = (color[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8;
                    img.put_pixel(px as u32, py as u32, Rgba([r, g, b, bg[3]]));
                }
            });
        }
        x_cursor += scaled.h_advance(gid);
        prev = Some(gid);
    }
}

/// Draw text scrolling horizontally within a clipped region, with seamless wrap-around.
/// `scroll_offset` is in pixels; the text repeats after `text_width + gap`.
fn draw_text_scrolling(
    img: &mut RgbaImage,
    text: &str,
    area_x: u32,
    area_y: u32,
    area_w: u32,
    size_px: f32,
    color: Rgba<u8>,
    scroll_offset: f32,
    text_width: f32,
    gap: f32,
) {
    let Some(font) = TITLE_FONT.as_ref() else {
        return;
    };
    if text.is_empty() {
        return;
    }

    let scale = PxScale::from(size_px);
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let y_baseline = area_y as f32 + ascent + 1.0;
    let cycle = text_width + gap;

    // Draw two copies of the text for seamless scrolling
    for copy in 0..2 {
        let x_start = area_x as f32 + 2.0 - scroll_offset + copy as f32 * cycle;

        // Skip if this copy is entirely off-screen
        if x_start > area_x as f32 + area_w as f32 {
            continue;
        }
        if x_start + text_width < area_x as f32 {
            continue;
        }

        let mut x_cursor = x_start;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for c in text.chars() {
            let gid = font.glyph_id(c);
            if let Some(p) = prev {
                x_cursor += scaled.kern(p, gid);
            }
            let glyph: Glyph = gid.with_scale_and_position(scale, point(x_cursor, y_baseline));

            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    // Clip to area_x..area_x+area_w horizontally
                    if px >= area_x as i32
                        && (px as u32) < area_x + area_w
                        && py >= 0
                        && (py as u32) < img.height()
                    {
                        let bg = *img.get_pixel(px as u32, py as u32);
                        let a = coverage * (color[3] as f32 / 255.0);
                        let r = (color[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8;
                        let g = (color[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8;
                        let b = (color[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8;
                        img.put_pixel(px as u32, py as u32, Rgba([r, g, b, bg[3]]));
                    }
                });
            }
            x_cursor += scaled.h_advance(gid);
            prev = Some(gid);
        }
    }
}

static VOLUME_BAR_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub static TRANSPARENT_ICON: LazyLock<String> = LazyLock::new(|| {
    const ICON_SIZE: u32 = 144;
    let img = RgbaImage::from_pixel(ICON_SIZE, ICON_SIZE, Rgba([0, 0, 0, 0]));

    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .expect("Failed to encode transparent icon");

    let base64 = general_purpose::STANDARD.encode(&buffer);
    format!("data:image/png;base64,{}", base64)
});

enum BarPosition {
    Upper,
    Lower,
}

impl fmt::Display for BarPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BarPosition::Upper => write!(f, "Upper"),
            BarPosition::Lower => write!(f, "Lower"),
        }
    }
}

/// Get data URI format for split volume bar images
pub fn get_volume_bar_data_uri_split(volume_percent: f32) -> Result<(String, String)> {
    let upper_key = generate_cache_key(volume_percent, BarPosition::Upper);
    let lower_key = generate_cache_key(volume_percent, BarPosition::Lower);

    if let (Ok(Some(cached_upper)), Ok(Some(cached_lower))) = (
        get_cached_value_safe(&upper_key),
        get_cached_value_safe(&lower_key),
    ) {
        return Ok((cached_upper, cached_lower));
    }

    let (top_base64, bottom_base64) = get_volume_bar_base64_split(volume_percent)?;
    let top_data_uri = format!("data:image/png;base64,{}", top_base64);
    let bottom_data_uri = format!("data:image/png;base64,{}", bottom_base64);

    set_cached_value(upper_key, top_data_uri.clone())
        .expect("Failed to retrieve cached upper part of volume bar");
    set_cached_value(lower_key, bottom_data_uri.clone())
        .expect("Failed to retrieve cached lower part of volume bar");

    Ok((top_data_uri, bottom_data_uri))
}

fn set_cached_value(key: String, value: String) -> Result<(), String> {
    match get_cache().lock() {
        Ok(mut cache) => {
            cache.insert(key, value);
            Ok(())
        }
        Err(_) => Err("Failed to acquire cache lock".to_string()),
    }
}

fn get_cache() -> &'static Mutex<HashMap<String, String>> {
    VOLUME_BAR_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn generate_cache_key(volume_percent: f32, position: BarPosition) -> String {
    format!("vol_{:.1}_part_{}", volume_percent, position)
}

fn get_cached_value_safe(key: &str) -> Result<Option<String>, String> {
    match get_cache().lock() {
        Ok(cache) => Ok(cache.get(key).cloned()),
        Err(_) => Err("Failed to acquire cache lock".to_string()),
    }
}

/// Blend two colors with alpha blending
fn blend_colors(bg: Rgba<u8>, fg: Rgba<u8>, alpha: f32) -> Rgba<u8> {
    let alpha = alpha.clamp(0.0, 1.0);

    // If background is fully transparent, just return foreground with adjusted alpha
    if bg[3] == 0 {
        return Rgba([fg[0], fg[1], fg[2], (fg[3] as f32 * alpha) as u8]);
    }

    let fg_alpha = (fg[3] as f32 / 255.0) * alpha;
    let bg_alpha = bg[3] as f32 / 255.0;
    let final_alpha = fg_alpha + bg_alpha * (1.0 - fg_alpha);

    if final_alpha == 0.0 {
        return Rgba([0, 0, 0, 0]);
    }

    let r = ((fg[0] as f32 * fg_alpha + bg[0] as f32 * bg_alpha * (1.0 - fg_alpha)) / final_alpha)
        as u8;
    let g = ((fg[1] as f32 * fg_alpha + bg[1] as f32 * bg_alpha * (1.0 - fg_alpha)) / final_alpha)
        as u8;
    let b = ((fg[2] as f32 * fg_alpha + bg[2] as f32 * bg_alpha * (1.0 - fg_alpha)) / final_alpha)
        as u8;
    let a = (final_alpha * 255.0) as u8;

    Rgba([r, g, b, a])
}

/// Calculate signed distance from a point to a rounded rectangle
/// Negative values mean inside, positive values mean outside
fn volume_bar_distance(
    px: f32,
    py: f32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> f32 {
    let dx = (px - x - width / 2.0).abs() - (width / 2.0 - radius);
    let dy = (py - y - height / 2.0).abs() - (height / 2.0 - radius);

    let outside_dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside_dist = dx.max(dy).min(0.0);

    outside_dist + inside_dist - radius
}

/// Generate a volume bar image spanning 2 Stream Deck icons (288x144 total)
/// Returns (top_image, bottom_image) as separate 144x144 images
pub fn generate_volume_bar_split(volume_percent: f32) -> (RgbaImage, RgbaImage) {
    const ICON_WIDTH: u32 = 144;
    const ICON_HEIGHT: u32 = 144;
    const TOTAL_HEIGHT: u32 = 288;
    const BAR_WIDTH: u32 = 26;
    const BAR_HEIGHT: u32 = 240;
    const POINTER_RADIUS: u32 = 20;
    const OUTLINE_THICKNESS: u32 = 6;

    let mut full_img = RgbaImage::from_pixel(ICON_WIDTH, TOTAL_HEIGHT, Rgba([0, 0, 0, 0]));

    let bar_x = (ICON_WIDTH - BAR_WIDTH) / 2;
    let bar_y = (TOTAL_HEIGHT - BAR_HEIGHT) / 2;

    let bar_fill = Rgba([255, 255, 255, 255]);
    let bar_outline = Rgba([255, 255, 255, 255]);
    let circle_outline = Rgba([255, 255, 255, 255]);

    draw_volume_bar_outline(
        &mut full_img,
        bar_x,
        bar_y,
        BAR_WIDTH,
        BAR_HEIGHT,
        BAR_WIDTH / 2,
        bar_outline,
        OUTLINE_THICKNESS,
    );

    // Calculate and draw the filled portion
    let fill_height = ((volume_percent / 100.0) * BAR_HEIGHT as f32) as u32;
    let fill_y = bar_y + BAR_HEIGHT - fill_height;

    if fill_height > OUTLINE_THICKNESS {
        for py in fill_y.max(bar_y + OUTLINE_THICKNESS)..(bar_y + BAR_HEIGHT - OUTLINE_THICKNESS) {
            for px in (bar_x + OUTLINE_THICKNESS)..(bar_x + BAR_WIDTH - OUTLINE_THICKNESS + 1) {
                if px < full_img.width() && py < full_img.height() {
                    full_img.put_pixel(px, py, bar_fill);
                }
            }
        }
    }

    // Draw the volume indicator circle
    let circle_x = bar_x + BAR_WIDTH / 2;
    let circle_y = fill_y;

    draw_volume_pointer(
        &mut full_img,
        circle_x,
        circle_y,
        POINTER_RADIUS,
        Rgba([0, 0, 0, 255]),
        circle_outline,
        OUTLINE_THICKNESS,
    );

    // Split into top and bottom images
    let mut top_img = RgbaImage::from_pixel(ICON_WIDTH, ICON_HEIGHT, Rgba([0, 0, 0, 0]));
    let mut bottom_img = RgbaImage::from_pixel(ICON_WIDTH, ICON_HEIGHT, Rgba([0, 0, 0, 0]));

    for y in 0..ICON_HEIGHT {
        for x in 0..ICON_WIDTH {
            top_img.put_pixel(x, y, *full_img.get_pixel(x, y));
            bottom_img.put_pixel(x, y, *full_img.get_pixel(x, y + ICON_HEIGHT));
        }
    }

    (top_img, bottom_img)
}

/// Draw a filled circle with outline and antialiasing
fn draw_volume_pointer(
    img: &mut RgbaImage,
    center_x: u32,
    center_y: u32,
    radius: u32,
    fill_color: Rgba<u8>,
    outline_color: Rgba<u8>,
    outline_thickness: u32,
) {
    let cx = center_x as f32;
    let cy = center_y as f32;
    let outer_r = radius as f32;
    let inner_r = (radius as f32) - (outline_thickness as f32);

    let min_x = (cx - outer_r - 1.0).max(0.0) as u32;
    let max_x = (cx + outer_r + 1.0).min(img.width() as f32) as u32;
    let min_y = (cy - outer_r - 1.0).max(0.0) as u32;
    let max_y = (cy + outer_r + 1.0).min(img.height() as f32) as u32;

    for py in min_y..max_y {
        for px in min_x..max_x {
            let dx = px as f32 - cx;
            let dy = py as f32 - cy;
            let distance = (dx * dx + dy * dy).sqrt();

            if distance <= outer_r {
                let bg = img.get_pixel(px, py);

                if distance >= inner_r {
                    // Outline region
                    let mut alpha: f32 = 1.0;

                    // AA for outer edge
                    if distance > outer_r - 1.0 {
                        alpha = alpha.min(outer_r - distance);
                    }
                    // AA for inner edge
                    if distance < inner_r + 1.0 {
                        alpha = alpha.min(distance - inner_r);
                    }

                    if alpha > 0.0 {
                        let blended = blend_colors(*bg, outline_color, alpha);
                        img.put_pixel(px, py, blended);
                    }
                } else {
                    // Fill region
                    let alpha = if distance >= inner_r - 1.0 {
                        (inner_r - distance).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };

                    if alpha > 0.0 {
                        let blended = blend_colors(*bg, fill_color, alpha);
                        img.put_pixel(px, py, blended);
                    }
                }
            }
        }
    }
}

/// Draw only the outline of a rounded rectangle with antialiasing
fn draw_volume_bar_outline(
    img: &mut RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    corner_radius: u32,
    outline_color: Rgba<u8>,
    outline_thickness: u32,
) {
    let x_f = x as f32;
    let y_f = y as f32;
    let width_f = width as f32;
    let height_f = height as f32;
    let r = corner_radius as f32;
    let thickness = outline_thickness as f32;

    let min_x = (x_f - 1.0).max(0.0) as u32;
    let max_x = (x_f + width_f + 1.0).min(img.width() as f32) as u32;
    let min_y = (y_f - 1.0).max(0.0) as u32;
    let max_y = (y_f + height_f + 1.0).min(img.height() as f32) as u32;

    for py in min_y..max_y {
        for px in min_x..max_x {
            let px_f = px as f32;
            let py_f = py as f32;

            let dist_outer = volume_bar_distance(px_f, py_f, x_f, y_f, width_f, height_f, r);

            let inner_x = x_f + thickness;
            let inner_y = y_f + thickness;
            let inner_width = width_f - thickness * 2.0;
            let inner_height = height_f - thickness * 2.0;
            let inner_r = (r - thickness).max(0.0);
            let dist_inner = volume_bar_distance(
                px_f,
                py_f,
                inner_x,
                inner_y,
                inner_width,
                inner_height,
                inner_r,
            );

            if dist_outer <= 0.0 && dist_inner > 0.0 {
                let mut alpha: f32 = 1.0;

                if dist_outer > -1.0 {
                    alpha = alpha.min(-dist_outer);
                }

                if dist_inner < 1.0 {
                    alpha = alpha.min(dist_inner);
                }

                if alpha > 0.0 {
                    let bg = img.get_pixel(px, py);
                    let blended = blend_colors(*bg, outline_color, alpha);
                    img.put_pixel(px, py, blended);
                }
            }
        }
    }
}

/// Get base64 encoded volume bar images for 2 vertical Stream Deck icons
fn get_volume_bar_base64_split(volume_percent: f32) -> Result<(String, String)> {
    let (top_img, bottom_img) = generate_volume_bar_split(volume_percent);

    let mut top_buffer = Vec::new();
    let mut top_cursor = Cursor::new(&mut top_buffer);
    top_img.write_to(&mut top_cursor, image::ImageFormat::Png)?;
    let top_base64 = general_purpose::STANDARD.encode(&top_buffer);

    let mut bottom_buffer = Vec::new();
    let mut bottom_cursor = Cursor::new(&mut bottom_buffer);
    bottom_img.write_to(&mut bottom_cursor, image::ImageFormat::Png)?;
    let bottom_base64 = general_purpose::STANDARD.encode(&bottom_buffer);

    Ok((top_base64, bottom_base64))
}

/// Draw a rounded-rect horizontal volume bar: track across the full width,
/// with the leftmost `fill_ratio` portion in `fill_color`.
fn draw_horizontal_volume_bar(
    img: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    track_color: Rgba<u8>,
    fill_color: Rgba<u8>,
    fill_ratio: f32,
) {
    let x_f = x as f32;
    let y_f = y as f32;
    let w_f = w as f32;
    let h_f = h as f32;
    let r = radius as f32;
    let fill_x_max = x_f + w_f * fill_ratio.clamp(0.0, 1.0);

    let min_x = x.saturating_sub(1);
    let max_x = (x + w + 1).min(img.width());
    let min_y = y.saturating_sub(1);
    let max_y = (y + h + 1).min(img.height());

    for py in min_y..max_y {
        for px in min_x..max_x {
            let px_f = px as f32 + 0.5;
            let py_f = py as f32 + 0.5;
            let d = volume_bar_distance(px_f, py_f, x_f, y_f, w_f, h_f, r);
            if d > 0.5 {
                continue;
            }
            let coverage = (0.5 - d).clamp(0.0, 1.0);
            let color = if px_f < fill_x_max { fill_color } else { track_color };
            let bg = *img.get_pixel(px, py);
            img.put_pixel(px, py, blend_colors(bg, color, coverage));
        }
    }
}

/// Generate a 200x100 encoder LCD image: icon on the left, title + horizontal
/// volume bar + percent readout on the right.
///
/// Layout (native canvas, top-left origin):
///   - Icon: 96x96, x=2..98, y=2..98
///   - Right column: x=102..198 (96px wide)
///       - Title strip: y=4..34, 24px Noto Sans Bold, centered, scrolls if overflowing
///       - Horizontal volume bar: x=108..192, y=44..64, rounded
///       - Percent readout ("NN%"): y=68..96, 24px, centered
///   - If muted: bar uses grey fill and the entire image is dimmed to ~35%.
pub fn get_encoder_lcd_data_uri(icon_data_uri: &str, title: &str, vol_percent: f32, muted: bool, scroll_offset: f32) -> Result<String> {
    const W: u32 = 200;
    const H: u32 = 100;

    const ICON_SIZE: u32 = 96;
    const ICON_X_OFF: i32 = 2;
    const ICON_Y_OFF: i32 = 2;

    const RIGHT_X: u32 = 102;
    const RIGHT_W: u32 = 96;
    const TITLE_PAD: u32 = 4;
    const TITLE_X: u32 = RIGHT_X + TITLE_PAD; // 106
    const TITLE_W: u32 = RIGHT_W - TITLE_PAD * 2; // 88
    const TITLE_Y: u32 = 4;
    const TITLE_SIZE: f32 = 24.0;

    const BAR_X: u32 = RIGHT_X + 6; // 108
    const BAR_W: u32 = RIGHT_W - 12; // 84
    const BAR_Y: u32 = 44;
    const BAR_H: u32 = 20;
    const BAR_RADIUS: u32 = 10;

    const PCT_Y: u32 = 68;
    const PCT_SIZE: f32 = 24.0;

    let mut img = RgbaImage::from_pixel(W, H, Rgba([18, 18, 18, 255]));

    // --- Title (top of right column, scrolling if needed) ---
    let title_color = Rgba([220, 220, 220, 255]);
    if scroll_offset > 0.0 {
        let font = TITLE_FONT.as_ref();
        let text_width = font
            .map(|f| measure_text_width(f, title, PxScale::from(TITLE_SIZE)))
            .unwrap_or(0.0);
        draw_text_scrolling(&mut img, title, TITLE_X, TITLE_Y, TITLE_W, TITLE_SIZE, title_color, scroll_offset, text_width, 30.0);
    } else {
        draw_text_centered(&mut img, title, TITLE_X, TITLE_Y, TITLE_W, TITLE_SIZE, title_color);
    }

    // --- App icon, left side ---
    let icon_bytes_b64 = icon_data_uri.split_once(',').map(|(_, b)| b).unwrap_or("");
    if let Ok(icon_bytes) = base64::engine::general_purpose::STANDARD.decode(icon_bytes_b64) {
        if let Ok(icon_img) = image::load_from_memory(&icon_bytes) {
            let icon = icon_img.resize(ICON_SIZE, ICON_SIZE, image::imageops::FilterType::Lanczos3);
            let icon_rgba = icon.to_rgba8();
            for (px, py, pixel) in icon_rgba.enumerate_pixels() {
                let x = px as i32 + ICON_X_OFF;
                let y = py as i32 + ICON_Y_OFF;
                if x >= 0 && y >= 0 && x < W as i32 && y < H as i32 {
                    let a = pixel[3] as f32 / 255.0;
                    let bg = img.get_pixel(x as u32, y as u32);
                    let r = (pixel[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8;
                    let g = (pixel[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8;
                    let b = (pixel[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8;
                    img.put_pixel(x as u32, y as u32, Rgba([r, g, b, 255]));
                }
            }
        }
    }

    // --- Horizontal volume bar ---
    let track_color = Rgba([40, 40, 40, 255]);
    let fill_color = if muted {
        Rgba([90, 90, 90, 255])
    } else {
        Rgba([80, 200, 120, 255])
    };
    let fill_ratio = (vol_percent / 100.0).clamp(0.0, 1.0);
    draw_horizontal_volume_bar(&mut img, BAR_X, BAR_Y, BAR_W, BAR_H, BAR_RADIUS, track_color, fill_color, fill_ratio);

    // --- Percent readout ---
    let pct_str = format!("{}%", vol_percent.round() as i32);
    draw_text_centered(&mut img, &pct_str, TITLE_X, PCT_Y, TITLE_W, PCT_SIZE, title_color);

    // --- Light rounded border around the LCD ---
    const BORDER_COLOR: Rgba<u8> = Rgba([200, 200, 200, 255]);
    const BORDER_RADIUS: u32 = 12;
    const BORDER_THICKNESS: u32 = 2;
    draw_volume_bar_outline(
        &mut img,
        0,
        0,
        W,
        H,
        BORDER_RADIUS,
        BORDER_COLOR,
        BORDER_THICKNESS,
    );

    // --- Mute dim overlay ---
    if muted {
        for pixel in img.pixels_mut() {
            pixel[0] = (pixel[0] as f32 * 0.35) as u8;
            pixel[1] = (pixel[1] as f32 * 0.35) as u8;
            pixel[2] = (pixel[2] as f32 * 0.35) as u8;
        }
    }

    // --- Clip everything outside the rounded border to transparent ---
    for y in 0..H {
        for x in 0..W {
            let d = volume_bar_distance(
                x as f32 + 0.5,
                y as f32 + 0.5,
                0.0,
                0.0,
                W as f32,
                H as f32,
                BORDER_RADIUS as f32,
            );
            if d >= 0.5 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            } else if d > -0.5 {
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                let px = img.get_pixel(x, y);
                let new_a = ((px[3] as f32) * coverage) as u8;
                img.put_pixel(x, y, Rgba([px[0], px[1], px[2], new_a]));
            }
        }
    }

    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)?;
    Ok(format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&buf)))
}

/// Render a 144x144 keypad image for a volume-memory button: the button's
/// label on top and its state ("empty" or how many sources it holds) below,
/// inside a rounded border that turns green once a snapshot is stored. The
/// `loaded` preset (last stored or recalled, and not changed since) gets a
/// filled green background and a thicker border so it stands out from its
/// siblings at a glance.
/// While the clear key is `armed`, every stored button is washed in a red
/// haze and reads "tap to clear" so the deck shows which keys a tap would
/// erase. `flash` inverts the face (bright background, dark glyph and
/// text, reading "saved") for the instant a hold has just stored.
pub fn get_memory_button_data_uri(
    label: &str,
    stored_sources: Option<usize>,
    loaded: bool,
    armed: bool,
    flash: bool,
) -> Result<String> {
    const SIZE: u32 = 144;
    const ICON_CY: f32 = 44.0;
    const ICON_SIZE: f32 = 48.0;
    const LABEL_Y: u32 = 72;
    const LABEL_SIZE: f32 = 26.0;
    const STATUS_Y: u32 = 104;
    const STATUS_SIZE: f32 = 19.0;
    const TEXT_PAD: u32 = 14;

    let stored = Rgba([80, 200, 120, 255]);
    let empty = Rgba([120, 120, 120, 255]);
    let danger = Rgba([235, 95, 95, 255]);
    let loaded = loaded && stored_sources.is_some();
    let armed = armed && stored_sources.is_some();

    let (background, accent, status_color, border_thickness) = if loaded {
        (
            Rgba([22, 78, 46, 255]),
            Rgba([140, 240, 180, 255]),
            Rgba([255, 255, 255, 255]),
            KEY_BORDER_THICKNESS + 2,
        )
    } else if stored_sources.is_some() {
        (Rgba([18, 18, 18, 255]), stored, stored, KEY_BORDER_THICKNESS)
    } else {
        (Rgba([18, 18, 18, 255]), empty, empty, KEY_BORDER_THICKNESS)
    };

    if flash {
        let bright = Rgba([225, 250, 235, 255]);
        let dark = Rgba([16, 48, 30, 255]);
        let mut img = RgbaImage::from_pixel(SIZE, SIZE, bright);
        draw_speaker_glyph(&mut img, SIZE as f32 / 2.0, ICON_CY, ICON_SIZE, dark);
        draw_text_centered(&mut img, label, TEXT_PAD, LABEL_Y, SIZE - TEXT_PAD * 2, LABEL_SIZE, dark);
        draw_text_centered(&mut img, "saved", TEXT_PAD, STATUS_Y, SIZE - TEXT_PAD * 2, STATUS_SIZE, dark);
        draw_key_frame(&mut img, stored, KEY_BORDER_THICKNESS + 2);
        return png_data_uri(&img);
    }

    let mut img = RgbaImage::from_pixel(SIZE, SIZE, background);

    let label_color = Rgba([235, 235, 235, 255]);
    draw_speaker_glyph(&mut img, SIZE as f32 / 2.0, ICON_CY, ICON_SIZE, label_color);
    draw_text_centered(&mut img, label, TEXT_PAD, LABEL_Y, SIZE - TEXT_PAD * 2, LABEL_SIZE, label_color);

    if armed {
        // Wash the whole face (icon and label included) in red, then put the
        // instruction on top in clean white so it stays readable.
        haze(&mut img, Rgba([225, 50, 50, 255]), 0.55);
        let white = Rgba([255, 255, 255, 255]);
        draw_text_centered(&mut img, "tap to clear", TEXT_PAD, STATUS_Y, SIZE - TEXT_PAD * 2, STATUS_SIZE, white);
        draw_key_frame(&mut img, danger, KEY_BORDER_THICKNESS + 2);
        return png_data_uri(&img);
    }

    let status = match (loaded, stored_sources) {
        (true, _) => "loaded".to_string(),
        (false, Some(1)) => "1 source".to_string(),
        (false, Some(n)) => format!("{n} sources"),
        (false, None) => "empty".to_string(),
    };
    draw_text_centered(&mut img, &status, TEXT_PAD, STATUS_Y, SIZE - TEXT_PAD * 2, STATUS_SIZE, status_color);

    draw_key_frame(&mut img, accent, border_thickness);
    png_data_uri(&img)
}

/// Blend `color` over every pixel at `opacity` (0..1), keeping alpha.
fn haze(img: &mut RgbaImage, color: Rgba<u8>, opacity: f32) {
    for px in img.pixels_mut() {
        let a = px[3];
        let mixed = blend_colors(*px, color, opacity);
        *px = Rgba([mixed[0], mixed[1], mixed[2], a]);
    }
}

/// How far the memory/clear key frame sits in from each image edge. Stream
/// Deck bezels hide the outermost pixels, so a border drawn flush to the
/// edge loses its outer rows; the right edge is hidden a few pixels deeper
/// than the left on the hardware this was tuned against.
const KEY_FRAME_INSET_LEFT: u32 = 3;
const KEY_FRAME_INSET_RIGHT: u32 = 7;
const KEY_FRAME_INSET_TOP: u32 = 3;
const KEY_FRAME_INSET_BOTTOM: u32 = 3;
const KEY_FRAME_RADIUS: u32 = 16;
const KEY_BORDER_THICKNESS: u32 = 6;

/// Outline the key inside the frame insets and make everything outside
/// that rounded rectangle transparent.
fn draw_key_frame(img: &mut RgbaImage, color: Rgba<u8>, thickness: u32) {
    let x = KEY_FRAME_INSET_LEFT;
    let y = KEY_FRAME_INSET_TOP;
    let w = img.width() - KEY_FRAME_INSET_LEFT - KEY_FRAME_INSET_RIGHT;
    let h = img.height() - KEY_FRAME_INSET_TOP - KEY_FRAME_INSET_BOTTOM;
    draw_volume_bar_outline(img, x, y, w, h, KEY_FRAME_RADIUS, color, thickness);
    clip_rounded_rect(img, x, y, w, h, KEY_FRAME_RADIUS);
}

/// Draw a speaker glyph (body, cone, two sound arcs) centred on (`cx`, `cy`)
/// and roughly `size` pixels wide, antialiased by 4x4 supersampling.
fn draw_speaker_glyph(img: &mut RgbaImage, cx: f32, cy: f32, size: f32, color: Rgba<u8>) {
    let u = size / 48.0; // design unit: the glyph is laid out on a 48-wide grid
    // Body: rectangle left of centre. Cone: trapezoid flaring to the right.
    let body = (cx - 22.0 * u, cy - 6.0 * u, cx - 12.0 * u, cy + 6.0 * u);
    let cone_left = cx - 12.0 * u;
    let cone_right = cx - 1.0 * u;
    let cone_half_near = 6.0 * u;
    let cone_half_far = 15.0 * u;
    // Arcs: centred on the cone mouth, sweeping ±45°.
    let arc_cx = cx - 2.0 * u;
    let arcs = [(8.0 * u, 3.0 * u), (16.0 * u, 3.0 * u)];
    let half_angle = std::f32::consts::FRAC_PI_4;

    let inside = |x: f32, y: f32| -> bool {
        if x >= body.0 && x <= body.2 && y >= body.1 && y <= body.3 {
            return true;
        }
        if x >= cone_left && x <= cone_right {
            let t = (x - cone_left) / (cone_right - cone_left);
            let half = cone_half_near + (cone_half_far - cone_half_near) * t;
            if (y - cy).abs() <= half {
                return true;
            }
        }
        let dx = x - arc_cx;
        let dy = y - cy;
        if dx > 0.0 {
            let dist = (dx * dx + dy * dy).sqrt();
            let angle = dy.atan2(dx).abs();
            if angle <= half_angle {
                for (r, t) in arcs {
                    if (dist - r).abs() <= t / 2.0 {
                        return true;
                    }
                }
            }
        }
        false
    };

    let x0 = (cx - size * 0.6).max(0.0) as u32;
    let x1 = ((cx + size * 0.6) as u32).min(img.width());
    let y0 = (cy - size * 0.5).max(0.0) as u32;
    let y1 = ((cy + size * 0.5) as u32).min(img.height());
    const SS: u32 = 4;
    for py in y0..y1 {
        for px in x0..x1 {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if inside(x, y) {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let a = hits as f32 / (SS * SS) as f32;
                let bg = *img.get_pixel(px, py);
                img.put_pixel(px, py, blend_colors(bg, color, a));
            }
        }
    }
}

/// What the "clear preset" key is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearFace {
    /// Grey "CLEAR / preset"; nothing pending.
    Idle,
    /// Red "Tap the button to clear it"; the next memory key tap erases.
    Armed,
    /// Bright red flash after a hold wiped every preset.
    AllCleared,
}

/// Render a 144x144 keypad image for the "clear preset" button.
pub fn get_clear_button_data_uri(face: ClearFace) -> Result<String> {
    const SIZE: u32 = 144;
    const TEXT_PAD: u32 = 14;

    let danger = Rgba([235, 95, 95, 255]);
    let idle = Rgba([120, 120, 120, 255]);
    let white = Rgba([235, 235, 235, 255]);

    let (background, accent, border_thickness) = match face {
        ClearFace::Idle => (Rgba([18, 18, 18, 255]), idle, KEY_BORDER_THICKNESS),
        ClearFace::Armed => (Rgba([70, 20, 20, 255]), danger, KEY_BORDER_THICKNESS + 2),
        ClearFace::AllCleared => (Rgba([200, 45, 45, 255]), white, KEY_BORDER_THICKNESS + 2),
    };
    let mut img = RgbaImage::from_pixel(SIZE, SIZE, background);
    let text_w = SIZE - TEXT_PAD * 2;

    match face {
        ClearFace::Idle => {
            draw_text_centered(&mut img, "CLEAR", TEXT_PAD, 34, text_w, 32.0, white);
            draw_text_centered(&mut img, "preset", TEXT_PAD, 84, text_w, 22.0, idle);
        }
        ClearFace::Armed => {
            const LINE_SIZE: f32 = 26.0;
            for (i, line) in ["Tap the", "button", "to clear it"].iter().enumerate() {
                let y = 26 + i as u32 * 32;
                draw_text_centered(&mut img, line, TEXT_PAD, y, text_w, LINE_SIZE, white);
            }
        }
        ClearFace::AllCleared => {
            draw_text_centered(&mut img, "ALL", TEXT_PAD, 34, text_w, 32.0, white);
            draw_text_centered(&mut img, "cleared", TEXT_PAD, 84, text_w, 22.0, white);
        }
    }
    draw_key_frame(&mut img, accent, border_thickness);
    png_data_uri(&img)
}

/// Make everything outside the rounded rectangle (`rx`, `ry`, `rw`, `rh`)
/// transparent, with antialiased edges.
fn clip_rounded_rect(img: &mut RgbaImage, rx: u32, ry: u32, rw: u32, rh: u32, radius: u32) {
    let (w, h) = (img.width(), img.height());
    for y in 0..h {
        for x in 0..w {
            let d = volume_bar_distance(
                x as f32 + 0.5,
                y as f32 + 0.5,
                rx as f32,
                ry as f32,
                rw as f32,
                rh as f32,
                radius as f32,
            );
            if d >= 0.5 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            } else if d > -0.5 {
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                let px = img.get_pixel(x, y);
                let new_a = ((px[3] as f32) * coverage) as u8;
                img.put_pixel(x, y, Rgba([px[0], px[1], px[2], new_a]));
            }
        }
    }
}

fn png_data_uri(img: &RgbaImage) -> Result<String> {
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)?;
    Ok(format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&buf)))
}

#[cfg(test)]
mod memory_button_tests {
    #[test]
    #[ignore = "writes preview PNGs for manual inspection"]
    fn dump_memory_button_previews() {
        let dir = std::env::var("MEMORY_PREVIEW_DIR").expect("MEMORY_PREVIEW_DIR");
        for (name, label, stored, loaded, armed, flash) in [
            ("empty", "Preset 1", None, false, false, false),
            ("stored", "Preset 2", Some(5usize), false, false, false),
            ("one", "Stream night", Some(1), false, false, false),
            ("loaded", "Preset 3", Some(5), true, false, false),
            ("armed", "Preset 3", Some(5), true, true, false),
            ("flash", "Preset 3", Some(5), true, false, true),
        ] {
            let uri = super::get_memory_button_data_uri(label, stored, loaded, armed, flash).unwrap();
            let b64 = uri.split_once(',').unwrap().1;
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
            std::fs::write(format!("{dir}/memory-{name}.png"), bytes).unwrap();
        }
        for (name, face) in [
            ("clear-idle", super::ClearFace::Idle),
            ("clear-armed", super::ClearFace::Armed),
            ("clear-all", super::ClearFace::AllCleared),
        ] {
            let uri = super::get_clear_button_data_uri(face).unwrap();
            let b64 = uri.split_once(',').unwrap().1;
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
            std::fs::write(format!("{dir}/memory-{name}.png"), bytes).unwrap();
        }
    }
}
