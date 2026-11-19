//! Brand logo + mascot sprite loading.
//!
//! The WebView frontend pulls these from a CDN at runtime:
//!   * `src/lib/brandLogos.ts` -> lobehub SVG icons + alrra browser-logos PNGs
//!   * `src/lib/AemeathAvatar.svelte` -> animated GIF sprite sheets
//!
//! For the native UI everything is bundled under `assets/` so the app never
//! touches the network at startup. The Grok and ChatGPT marks were inline
//! data-URIs in `brandLogos.ts`; they are kept inline here for the same
//! reason.

use std::path::PathBuf;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

pub fn asset_dir() -> PathBuf {
    // 1) exe/assets          -> release deploy
    // 2) CARGO_MANIFEST_DIR  -> `cargo run` from any working directory
    let mut p = std::env::current_exe().unwrap_or_default();
    p.pop();
    let candidate = p.join("assets");
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
}

/// Discovers audio files bundled in `assets/musik/` (sorted by filename).
pub fn music_tracks() -> Vec<PathBuf> {
    let mut dir = asset_dir().join("musik");
    if !dir.is_dir() {
        let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets").join("musik");
        if fallback.is_dir() {
            dir = fallback;
        } else {
            return Vec::new();
        }
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut tracks: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "mp3" | "wav" | "wma" | "m4a"))
                    .unwrap_or(false)
        })
        .collect();
    tracks.sort();
    tracks
}

fn load_image(rel: &str) -> Image {
    let path = asset_dir().join(rel);
    Image::load_from_path(&path).unwrap_or_default()
}

/// Load a PNG and bake a mirror/flip into its pixels. The software renderer
/// used by this app ignores transform-scale-* properties, so edge poses must
/// be pre-transformed instead of relying on Slint transforms.
fn load_image_transformed(rel: &str, mirror: bool, flip_vertical: bool) -> Image {
    let path = asset_dir().join(rel);
    let Ok(source) = image::open(&path) else {
        return Image::default();
    };
    let mut rgba = source.to_rgba8();
    if mirror {
        image::imageops::flip_horizontal_in_place(&mut rgba);
    }
    if flip_vertical {
        image::imageops::flip_vertical_in_place(&mut rgba);
    }
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    Image::from_rgba8(buffer)
}

fn load_svg(rel: &str) -> Image {
    let path = asset_dir().join(rel);
    match std::fs::read(&path) {
        Ok(bytes) => {
            // CDN svgs use `width/height="1em"` — resvg resolves em to 0 -> blank.
            // Normalise to a concrete pixel size before handing over to Slint.
            let markup = String::from_utf8_lossy(&bytes)
                .replace("1em", "48")
                .replace("style=\"flex:none;line-height:1\"", "");
            Image::load_from_svg_data(markup.as_bytes()).unwrap_or_default()
        }
        Err(_) => Image::default(),
    }
}

// ---------------------------------------------------------------------------
//  Inline marks (were data-URIs in brandLogos.ts)
// ---------------------------------------------------------------------------
const GROK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#FFFFFF" d="M9.27 15.29l7.978-5.897c.391-.29.95-.177 1.137.272.98 2.369.542 5.215-1.41 7.169-1.951 1.954-4.667 2.382-7.149 1.406l-2.711 1.257c3.889 2.661 8.611 2.003 11.562-.953 2.341-2.344 3.066-5.539 2.388-8.42l.006.007c-.983-4.232.242-5.924 2.75-9.383.06-.082.12-.164.179-.248l-3.301 3.305v-.01L9.267 15.292M7.623 16.723c-2.792-2.67-2.31-6.801.071-9.184 1.761-1.763 4.647-2.483 7.166-1.425l2.705-1.25a7.808 7.808 0 00-1.829-1A8.975 8.975 0 005.984 5.83c-2.533 2.536-3.33 6.436-1.962 9.764 1.022 2.487-.653 4.246-1.682 1.925l7.62-6.815"/></svg>"##;

const CHATGPT_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#10a37f" d="M9.205 8.658v-2.26c0-.19.072-.333.238-.428l4.543-2.616c.619-.357 1.356-.523 2.117-.523 2.854 0 4.662 2.212 4.662 4.566 0 .167 0 .357-.024.547l-4.71-2.759a.797.797 0 00-.856 0l-5.97 3.473zm10.609 8.8V12.06c0-.333-.143-.57-.429-.737l-5.97-3.473 1.95-1.118a.433.433 0 01.476 0l4.543 2.617c1.309.76 2.189 2.378 2.189 3.948 0 1.808-1.07 3.473-2.76 4.163zM7.802 12.703l-1.95-1.142a.433.433 0 01-.476 0l4.543-2.616c.619-.357 1.356-.524 2.117-.524 2.854 0 4.662 2.212 4.662 4.566 0 .167 0 .357-.024.547l-4.71-2.759a.797.797 0 00-.856 0l-5.97 3.473zm.17 11.658c-1 0-1.927-.332-2.712-.927l4.686-2.712c.285-.166.428-.404.428-.737v-6.898l1.974 1.142c.167.095.238.238.238.428v5.233c0 2.545-1.974 4.472-4.614 4.472zm-5.637-5.303l-4.544-2.617c-1.308-.761-2.188-2.378-2.188-3.948A4.482 4.482 0 014.21 6.327v5.423c0 2.545 1.95 4.472 4.591 4.472 1 0 1.927-.333 2.712-.928L8.23 5.067c-.285.166-.428.404-.428.737v6.898c0 2.545-1.974 4.472-4.614 4.472zm5.899 2.83a5.947 5.947 0 005.827-4.756C22.287 18.339 24 15.84 24 13.296c0-1.665-.713-3.282-1.998-4.448.119-.5.19-.999.19-1.498 0-3.401-2.759-5.947-5.946-5.947-.642 0-1.26.095-1.88.31A5.962 5.962 0 0010.205 0a5.947 5.947 0 00-5.827 4.757C1.713 5.447 0 7.945 0 10.49c0 1.666.713 3.283 1.998 4.448-.119.5-.19 1-.19 1.499 0 3.401 2.759 5.946 5.946 5.946.642 0 1.26-.095 1.88-.309A5.96 5.96 0 004.162 21.713z"/></svg>"##;

fn inline_svg(markup: &str) -> Image {
    Image::load_from_svg_data(markup.as_bytes()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
//  Public API
// ---------------------------------------------------------------------------

/// AI provider marks (indices match `ProviderItem.logo_index` in `model.rs` / Slint).
pub fn ai_logos() -> Vec<Image> {
    vec![
        inline_svg(GROK_SVG),                 // grok
        load_svg("logos/claude-color.svg"),   // claude
        load_svg("logos/gemini-color.svg"),   // gemini
        inline_svg(CHATGPT_SVG),              // chatgpt
        load_svg("logos/deepseek-color.svg"), // deepseek
    ]
}

/// Browser marks, in the same order as `model::seed_browsers()`.
pub fn browser_logos() -> Vec<Image> {
    vec![
        load_image("logos/chrome_48x48.png"),
        load_image("logos/edge_48x48.png"),
        load_image("logos/brave_48x48.png"),
        load_image("logos/firefox_48x48.png"),
    ]
}

/// The four frames of the breathing mascot, animated by the Slint Timer.
/// These are pre-baked 96x96 sprites: browser-exact pixelated contain scaling
/// plus the cyan drop-shadow, with the 64px img box at (16,16).
pub fn avatar_frames() -> Vec<Image> {
    (1..=4).map(|i| load_image(&format!("avatar/middle-{i}-96.png"))).collect()
}

/// Top-edge variant: the middle pose flipped vertically, matching the old
/// webview's scaleY(-1) when the bubble docks to the top.
pub fn avatar_frames_flipped_vertical() -> Vec<Image> {
    (1..=4)
        .map(|i| load_image_transformed(&format!("avatar/middle-{i}-96.png"), false, true))
        .collect()
}

/// The side-facing breathing frames (`/img/breathing/breathing.gif`). Used by
/// the floating bubble when it is docked to the left monitor edge.
pub fn avatar_side_frames() -> Vec<Image> {
    (1..=4)
        .map(|i| load_image(&format!("avatar/side-{i}-96.png")))
        .collect()
}

/// Right-edge variant of the side pose, horizontally mirrored.
pub fn avatar_side_frames_mirrored() -> Vec<Image> {
    (1..=4)
        .map(|i| load_image_transformed(&format!("avatar/side-{i}-96.png"), true, false))
        .collect()
}

/// Middle-facing singing frames (ms1..ms3 -> 96x96), lazy-loaded on first song.
pub fn avatar_sing_frames() -> Vec<Image> {
    (1..=3)
        .map(|i| load_image(&format!("avatar/sing-middle-{i}-96.png")))
        .collect()
}

/// Top-edge variant of the singing middle pose, flipped vertically.
pub fn avatar_sing_frames_flipped_vertical() -> Vec<Image> {
    (1..=3)
        .map(|i| load_image_transformed(&format!("avatar/sing-middle-{i}-96.png"), false, true))
        .collect()
}

/// Side-facing singing frames (sf1..sf3 -> 96x96), used when docked on left edge.
pub fn avatar_sing_side_frames() -> Vec<Image> {
    (1..=3)
        .map(|i| load_image(&format!("avatar/sing-side-{i}-96.png")))
        .collect()
}

/// Right-edge variant of the side singing pose, horizontally mirrored.
pub fn avatar_sing_side_frames_mirrored() -> Vec<Image> {
    (1..=3)
        .map(|i| load_image_transformed(&format!("avatar/sing-side-{i}-96.png"), true, false))
        .collect()
}

/// The twelve frames of `/img/hold/hold.gif`, shown while the bubble is
/// grabbed. The old webview swaps to this GIF on pointer-down so the character
/// visibly hangs on while being dragged.
pub fn avatar_hold_frames() -> Vec<Image> {
    (1..=12)
        .map(|i| load_image(&format!("avatar/hold-{i:02}-96.png")))
        .collect()
}

/// Panel-only 4x renders of the same four GIF frames at the 36 px web size,
/// including the browser's inner drop-shadow. The 24 px transparent margin
/// keeps the shadow unclipped when the asset is drawn at 84x84 around a 36x36
/// avatar box.
pub fn avatar_hi_frames() -> Vec<Image> {
    (1..=4).map(|i| load_image(&format!("avatar/middle-{i}-hi.png"))).collect()
}

/// Panel brand-only 4x renders with both the inner image shadow and the
/// `.brand-avatar-box` outer drop-shadow baked by Chromium.
pub fn avatar_hi_brand_frames() -> Vec<Image> {
    (1..=4).map(|i| load_image(&format!("avatar/middle-{i}-brand.png"))).collect()
}

fn load_image_resized(rel: &str, target_px: u32) -> Image {
    let path = asset_dir().join(rel);
    let Ok(source) = image::open(&path) else {
        return Image::default();
    };
    let rgba = source.to_rgba8();
    let resized = if rgba.width() != target_px || rgba.height() != target_px {
        image::imageops::resize(
            &rgba,
            target_px.max(16),
            target_px.max(16),
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        rgba
    };
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        resized.as_raw(),
        resized.width(),
        resized.height(),
    );
    Image::from_rgba8(buffer)
}

/// HUD header 28px avatar frames (`28 + 48 = 76` logical box with 24px glow padding),
/// Lanczos3-downscaled to exact physical pixels so the software renderer blits 1:1.
pub fn avatar_hud_frames(scale: f32) -> Vec<Image> {
    let target = (76.0 * scale.clamp(1.0, 3.0)).round() as u32;
    (1..=4)
        .map(|i| load_image_resized(&format!("avatar/middle-{i}-hi.png"), target))
        .collect()
}

/// The first HUD header avatar frame, sized 1:1 for the current DPI scale.
pub fn avatar_hud_sprite(scale: f32) -> Image {
    let target = (76.0 * scale.clamp(1.0, 3.0)).round() as u32;
    load_image_resized("avatar/middle-1-hi.png", target)
}

/// The first frame, used as a static placeholder.
pub fn avatar_sprite() -> Image {
    load_image("avatar/middle-1-96.png")
}

/// The first 4x panel frame.
pub fn avatar_hi_sprite() -> Image {
    load_image("avatar/middle-1-hi.png")
}

/// The first 4x panel brand frame with the outer glow included.
pub fn avatar_hi_brand_sprite() -> Image {
    load_image("avatar/middle-1-brand.png")
}
