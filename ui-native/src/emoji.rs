//! Color emoji rasterizer (Windows Direct2D + DirectWrite COLRv1 Fluent Emoji)
//! and Unicode emoji cluster / shortcode helpers.
//!
//! Slint's software renderer (`i-slint-renderer-software`) rasterizes inline
//! font runs as monochrome alpha masks, so standard color fonts fall back to
//! wireframes. By rendering emoji clusters once via Windows Direct2D
//! (`D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT`) onto a `IWICBitmap` and caching
//! them as `slint::Image`, standalone emoji reactions, heading badges, and list
//! icons display full-color Fluent 3D graphics with near-zero CPU cost.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static EMOJI_CACHE: RefCell<HashMap<(String, u32), Option<slint::Image>>> =
        RefCell::new(HashMap::new());
}

#[cfg(windows)]
struct D2dEmojiEngine {
    wic: windows::Win32::Graphics::Imaging::IWICImagingFactory,
    d2d: windows::Win32::Graphics::Direct2D::ID2D1Factory,
    dwrite: windows::Win32::Graphics::DirectWrite::IDWriteFactory,
}

#[cfg(windows)]
thread_local! {
    static D2D_ENGINE: RefCell<Option<Result<D2dEmojiEngine, ()>>> = const { RefCell::new(None) };
}

#[cfg(windows)]
fn with_engine<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&D2dEmojiEngine) -> Option<R>,
{
    D2D_ENGINE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        if borrow.is_none() {
            *borrow = Some(init_engine());
        }
        match borrow.as_ref()? {
            Ok(eng) => f(eng),
            Err(()) => None,
        }
    })
}

#[cfg(windows)]
fn init_engine() -> Result<D2dEmojiEngine, ()> {
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Factory, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
    };
    use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory, IWICImagingFactory};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(|_| ())?;
        let d2d: ID2D1Factory =
            D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).map_err(|_| ())?;
        let dwrite: IDWriteFactory =
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|_| ())?;
        Ok(D2dEmojiEngine { wic, d2d, dwrite })
    }
}

fn dist_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let denom = (bax * bax + bay * bay).max(1e-6);
    let h = ((pax * bax + pay * bay) / denom).clamp(0.0, 1.0);
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt()
}

fn sd_round_box(px: f32, py: f32, half: f32, radius: f32) -> f32 {
    let qx = (px - 0.5).abs() - (half - radius);
    let qy = (py - 0.5).abs() - (half - radius);
    let out_x = qx.max(0.0);
    let out_y = qy.max(0.0);
    (out_x * out_x + out_y * out_y).sqrt() + qx.max(qy).min(0.0) - radius
}

/// Renders a 4x supersampled anti-aliased vector badge for task/status markers:
/// - `"done"`: vibrant emerald green badge with crisp white checkmark (✅)
/// - `"todo"`: matching dark-slate rounded checkbox with empty center (non-checklist)
/// - `"cross"`: vibrant crimson red badge with crisp white cross (❌)
pub fn render_status_badge(kind: &str, px: u32) -> slint::Image {
    let px = px.clamp(16, 128);
    let mode = match kind {
        "done" | "check" => "done",
        "cross" | "fail" | "x" => "cross",
        _ => "todo",
    };
    let cache_key = (format!("__status_badge_{mode}"), px);
    if let Some(Some(img)) = EMOJI_CACHE.with(|c| c.borrow().get(&cache_key).cloned()) {
        return img;
    }

    let mut rgba = vec![0u8; (px * px * 4) as usize];
    let samples = 4u32;
    let inv_grid = 1.0 / ((px * samples) as f32);

    for y in 0..px {
        for x in 0..px {
            let mut sum_r = 0.0f32;
            let mut sum_g = 0.0f32;
            let mut sum_b = 0.0f32;
            let mut sum_a = 0.0f32;

            for sy in 0..samples {
                for sx in 0..samples {
                    let u = ((x * samples + sx) as f32 + 0.5) * inv_grid;
                    let v = ((y * samples + sy) as f32 + 0.5) * inv_grid;
                    let d_box = sd_round_box(u, v, 0.41, 0.21);
                    if d_box > 0.0 {
                        continue;
                    }

                    let t = ((v - 0.09) / 0.82).clamp(0.0, 1.0);
                    let (r, g, b, a) = match mode {
                        "done" => {
                            let d_mark = dist_segment(u, v, 0.285, 0.515, 0.435, 0.67)
                                .min(dist_segment(u, v, 0.435, 0.67, 0.725, 0.335));
                            if d_mark <= 0.068 {
                                (255.0, 255.0, 255.0, 1.0)
                            } else {
                                let mut br = 34.0 + (5.0 - 34.0) * t;
                                let mut bg = 211.0 + (150.0 - 211.0) * t;
                                let mut bb = 136.0 + (105.0 - 136.0) * t;
                                if d_box > -0.05 {
                                    br = br * 0.65 + 110.0 * 0.35;
                                    bg = bg * 0.65 + 231.0 * 0.35;
                                    bb = bb * 0.65 + 183.0 * 0.35;
                                }
                                (br, bg, bb, 1.0)
                            }
                        }
                        "cross" => {
                            let d_mark = dist_segment(u, v, 0.325, 0.325, 0.675, 0.675)
                                .min(dist_segment(u, v, 0.675, 0.325, 0.325, 0.675));
                            if d_mark <= 0.064 {
                                (255.0, 255.0, 255.0, 1.0)
                            } else {
                                let mut br = 244.0 + (185.0 - 244.0) * t;
                                let mut bg = 72.0 + (28.0 - 72.0) * t;
                                let mut bb = 72.0 + (28.0 - 72.0) * t;
                                if d_box > -0.05 {
                                    br = br * 0.7 + 252.0 * 0.3;
                                    bg = bg * 0.7 + 165.0 * 0.3;
                                    bb = bb * 0.7 + 165.0 * 0.3;
                                }
                                (br, bg, bb, 1.0)
                            }
                        }
                        _ => {
                            if d_box > -0.088 {
                                let br = 152.0 + (108.0 - 152.0) * t;
                                let bg = 178.0 + (132.0 - 178.0) * t;
                                let bb = 210.0 + (164.0 - 210.0) * t;
                                (br, bg, bb, 0.96)
                            } else {
                                let br = 22.0 + (13.0 - 22.0) * t;
                                let bg = 34.0 + (20.0 - 34.0) * t;
                                let bb = 56.0 + (36.0 - 56.0) * t;
                                (br, bg, bb, 0.85)
                            }
                        }
                    };
                    sum_r += r * a;
                    sum_g += g * a;
                    sum_b += b * a;
                    sum_a += a;
                }
            }

            let d = ((y * px + x) * 4) as usize;
            if sum_a > 0.0 {
                rgba[d] = (sum_r / sum_a).round().clamp(0.0, 255.0) as u8;
                rgba[d + 1] = (sum_g / sum_a).round().clamp(0.0, 255.0) as u8;
                rgba[d + 2] = (sum_b / sum_a).round().clamp(0.0, 255.0) as u8;
                rgba[d + 3] = ((sum_a / 16.0) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, px, px);
    let img = slint::Image::from_rgba8(buffer);
    EMOJI_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= 160 {
            map.clear();
        }
        map.insert(cache_key, Some(img.clone()));
    });
    img
}

/// Rasterizes a Unicode emoji cluster at `px` x `px` physical pixels via
/// Direct2D/DirectWrite (`Segoe UI Emoji`, COLR color layers enabled), with
/// crisp colored vector badges for ✅, ☐, and ❌ status markers.
#[cfg(windows)]
pub fn render_color_emoji(emoji: &str, px: u32) -> Option<slint::Image> {
    let trimmed = emoji.trim();
    if trimmed.is_empty() || !(8..=256).contains(&px) {
        return None;
    }

    let base = trimmed.trim_end_matches(['\u{FE0F}', '\u{FE0E}']);
    match base {
        "✅" | "☑" | "✔" => return Some(render_status_badge("done", px)),
        "☐" | "⬜" | "◻" | "◽" => return Some(render_status_badge("todo", px)),
        "❌" | "❎" | "✖" | "✘" | "☒" => return Some(render_status_badge("cross", px)),
        _ => {}
    }

    let key = (trimmed.to_string(), px);
    if let Some(cached) = EMOJI_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return cached;
    }

    let img = with_engine(|eng| rasterize_emoji_inner(eng, trimmed, px));
    EMOJI_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= 160 {
            map.clear();
        }
        map.insert(key, img.clone());
    });
    img
}

#[cfg(not(windows))]
pub fn render_color_emoji(_emoji: &str, _px: u32) -> Option<slint::Image> {
    None
}

#[cfg(windows)]
fn rasterize_emoji_inner(eng: &D2dEmojiEngine, emoji: &str, px: u32) -> Option<slint::Image> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_RENDER_TARGET_PROPERTIES,
        D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
        DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        DWRITE_TEXT_ALIGNMENT_CENTER,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::{
        GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad, WICBitmapLockRead, WICRect,
    };

    unsafe {
        let ss = (px * 2).clamp(px, 256);
        let bitmap = eng
            .wic
            .CreateBitmap(ss, ss, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)
            .ok()?;

        let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };

        let rt = eng.d2d.CreateWicBitmapRenderTarget(&bitmap, &rt_props).ok()?;
        let brush = rt
            .CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )
            .ok()?;

        let font_name: Vec<u16> = "Segoe UI Emoji\0".encode_utf16().collect();
        let locale: Vec<u16> = "en-US\0".encode_utf16().collect();
        let font_size = (ss as f32) * 0.80;
        let fmt = eng
            .dwrite
            .CreateTextFormat(
                PCWSTR(font_name.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size,
                PCWSTR(locale.as_ptr()),
            )
            .ok()?;
        let _ = fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);

        let text_u16: Vec<u16> = emoji.encode_utf16().collect();
        let rect = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: ss as f32,
            bottom: ss as f32,
        };

        rt.BeginDraw();
        rt.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }));
        rt.DrawText(
            &text_u16,
            &fmt,
            &rect,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        rt.EndDraw(None, None).ok()?;

        let wic_rect = WICRect {
            X: 0,
            Y: 0,
            Width: ss as i32,
            Height: ss as i32,
        };
        let lock = bitmap.Lock(&wic_rect, WICBitmapLockRead.0 as u32).ok()?;
        let stride = lock.GetStride().ok()? as usize;
        let mut buf_size = 0u32;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        lock.GetDataPointer(&mut buf_size, &mut ptr).ok()?;
        if ptr.is_null() || buf_size == 0 {
            return None;
        }
        let raw = std::slice::from_raw_parts(ptr, buf_size as usize);

        // Keep premultiplied RGBA while resizing with Lanczos3 so transparent glows never pick up dark halos.
        let mut pm_rgba = vec![0u8; (ss * ss * 4) as usize];
        let mut max_alpha = 0u8;
        for y in 0..ss as usize {
            let row_start = y * stride;
            for x in 0..ss as usize {
                let s = row_start + x * 4;
                if s + 3 >= raw.len() {
                    break;
                }
                let a = raw[s + 3];
                if a > max_alpha {
                    max_alpha = a;
                }
                let d = (y * (ss as usize) + x) * 4;
                pm_rgba[d] = raw[s + 2];
                pm_rgba[d + 1] = raw[s + 1];
                pm_rgba[d + 2] = raw[s];
                pm_rgba[d + 3] = a;
            }
        }
        if max_alpha == 0 {
            return None;
        }

        let final_pm = if ss != px {
            let src_img = image::RgbaImage::from_raw(ss, ss, pm_rgba)?;
            image::imageops::resize(&src_img, px, px, image::imageops::FilterType::Lanczos3)
                .into_raw()
        } else {
            pm_rgba
        };

        let mut rgba = vec![0u8; (px * px * 4) as usize];
        for (src, dst) in final_pm.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            let a = src[3];
            if a <= 2 {
                dst.fill(0);
            } else if a == 255 {
                dst[0] = src[0];
                dst[1] = src[1];
                dst[2] = src[2];
                dst[3] = 255;
            } else {
                let af = a as u16;
                dst[0] = (((src[0] as u16) * 255) / af).min(255) as u8;
                dst[1] = (((src[1] as u16) * 255) / af).min(255) as u8;
                dst[2] = (((src[2] as u16) * 255) / af).min(255) as u8;
                dst[3] = a;
            }
        }

        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, px, px);
        Some(slint::Image::from_rgba8(buffer))
    }
}

fn is_emoji_base(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x1F300..=0x1F5FF
            | 0x1F600..=0x1F64F
            | 0x1F680..=0x1F6FF
            | 0x1F7E0..=0x1F7EB
            | 0x1F900..=0x1F9FF
            | 0x1FA70..=0x1FAFF
            | 0x231A..=0x231B
            | 0x23E9..=0x23F3
            | 0x23F8..=0x23FA
            | 0x2610..=0x2612
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267F
            | 0x2693
            | 0x26A1
            | 0x26AA..=0x26AB
            | 0x26BD..=0x26BE
            | 0x26C4..=0x26C5
            | 0x26CE
            | 0x26D4
            | 0x26EA
            | 0x26F2..=0x26F5
            | 0x26FA
            | 0x26FD
            | 0x25FB..=0x25FE
            | 0x2705
            | 0x2708..=0x270D
            | 0x2714
            | 0x2716
            | 0x2718
            | 0x2728
            | 0x274C
            | 0x274E
            | 0x2753..=0x2755
            | 0x2757
            | 0x2764
            | 0x2795..=0x2797
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
    )
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

fn is_skin_tone(c: char) -> bool {
    matches!(c as u32, 0x1F3FB..=0x1F3FF)
}

fn is_keycap_base(c: char) -> bool {
    c.is_ascii_digit() || c == '#' || c == '*'
}

/// Attempts to scan one complete emoji grapheme cluster starting at byte 0 of `s`.
/// Returns `(cluster_slice, rest_slice)`.
pub fn take_emoji_cluster(s: &str) -> Option<(&str, &str)> {
    let mut it = s.char_indices();
    let (_, first) = it.next()?;

    // Regional indicator flag pair (e.g. 🇮🇩)
    if is_regional_indicator(first) {
        if let Some((idx2, ch2)) = it.next() {
            if is_regional_indicator(ch2) {
                let end = idx2 + ch2.len_utf8();
                return Some((&s[..end], &s[end..]));
            }
        }
        return None;
    }

    // Keycap sequence (#️⃣, 1️⃣)
    if is_keycap_base(first) {
        if let Some((idx2, ch2)) = it.next() {
            if ch2 == '\u{20E3}' {
                let end = idx2 + ch2.len_utf8();
                return Some((&s[..end], &s[end..]));
            }
            if ch2 == '\u{FE0F}' {
                if let Some((idx3, '\u{20E3}')) = it.next() {
                    let end = idx3 + '\u{20E3}'.len_utf8();
                    return Some((&s[..end], &s[end..]));
                }
            }
        }
        return None;
    }

    // Misc symbols/dingbats explicitly using VS16 selector (\u{FE0F}), e.g. ⚙️, 🖼️, ☁️
    let mut end = if is_emoji_base(first) {
        first.len_utf8()
    } else if matches!(first as u32, 0x2600..=0x27BF | 0x2194..=0x21AA | 0x2934..=0x2935) {
        if let Some((idx2, '\u{FE0F}')) = it.next() {
            idx2 + '\u{FE0F}'.len_utf8()
        } else {
            return None;
        }
    } else {
        return None;
    };

    // Consume trailing modifiers, VS16, and ZWJ chained components (e.g. 👨‍💻, ❤️‍🔥)
    loop {
        let tail = &s[end..];
        let Some(next) = tail.chars().next() else {
            break;
        };
        if next == '\u{FE0F}' || next == '\u{FE0E}' || is_skin_tone(next) {
            end += next.len_utf8();
            continue;
        }
        if next == '\u{200D}' {
            let after_zwj = &s[end + next.len_utf8()..];
            if let Some((comp, _)) = take_emoji_cluster(after_zwj) {
                end += next.len_utf8() + comp.len();
                continue;
            }
        }
        break;
    }

    Some((&s[..end], &s[end..]))
}

/// If `text` starts with a Unicode emoji cluster followed by whitespace and
/// non-empty remaining text, splits it out for a color-emoji accent badge.
pub fn split_leading_emoji(text: &str) -> Option<(String, &str)> {
    let trimmed = text.trim_start();
    let (emoji, rest) = take_emoji_cluster(trimmed)?;
    let has_gap = rest.chars().next().is_some_and(|c| c.is_whitespace());
    let after = rest.trim_start();
    if !has_gap || after.is_empty() {
        return None;
    }
    Some((emoji.to_string(), after))
}

/// Returns `Some(clusters)` if `line` consists exclusively of 1..=6 emoji
/// clusters (separated only by optional spaces), ideal for a standalone visual
/// color emoji reaction row.
pub fn extract_pure_emoji_row(line: &str) -> Option<Vec<String>> {
    let mut cur = line.trim();
    if cur.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    while !cur.is_empty() {
        let (cluster, rest) = take_emoji_cluster(cur)?;
        out.push(cluster.to_string());
        if out.len() > 6 {
            return None;
        }
        cur = rest.trim_start();
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Strips inline Unicode color emoji clusters outside backtick spans so
/// Slint's monochrome software font renderer never draws wireframe outlines,
/// returning `(cleaned_text, unique_clusters)` for crisp Direct2D rendering.
pub fn extract_inline_emojis(text: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len());
    let mut clusters: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut in_code = false;

    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with('`') {
            in_code = !in_code;
            out.push('`');
            i += 1;
            continue;
        }
        if !in_code {
            if let Some((cluster, _)) = take_emoji_cluster(rest) {
                let base = cluster.trim_end_matches(['\u{FE0F}', '\u{FE0E}']);
                let is_status_glyph = matches!(
                    base,
                    "✅" | "☑" | "✔" | "☐" | "⬜" | "◻" | "◽" | "❌" | "❎" | "✖" | "✘" | "☒"
                );
                if !is_status_glyph {
                    if !clusters.iter().any(|c| c == cluster) && clusters.len() < 8 {
                        clusters.push(cluster.to_string());
                    }
                    let tail_after = &rest[cluster.len()..];
                    // Avoid leaving a dangling space right before punctuation or double spaces.
                    if out.ends_with(' ')
                        && tail_after
                            .chars()
                            .next()
                            .is_some_and(|c| c == ' ' || matches!(c, '.' | ',' | '!' | '?' | ':' | ';' | ')'))
                    {
                        out.pop();
                    }
                    i += cluster.len();
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out.trim().to_string(), clusters)
}

/// Expands common emoji shortcodes (`:rocket:`, `:fire:`, `:sparkles:`, etc.)
/// outside inline code spans so models emitting Slack/GitHub shortcodes render
/// crisp Unicode emojis too.
pub fn expand_shortcodes(input: &str) -> String {
    if !input.contains(':') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut in_code = false;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            in_code = !in_code;
            out.push('`');
            i += 1;
            continue;
        }
        if !in_code && bytes[i] == b':' {
            if let Some(rel_end) = input[i + 1..].find(':') {
                let name = &input[i + 1..i + 1 + rel_end];
                if !name.is_empty()
                    && name.len() <= 24
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'+')
                {
                    if let Some(em) = shortcode_to_emoji(name) {
                        out.push_str(em);
                        i += 1 + rel_end + 1;
                        continue;
                    }
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn shortcode_to_emoji(name: &str) -> Option<&'static str> {
    Some(match name {
        "rocket" => "🚀",
        "sparkles" => "✨",
        "fire" => "🔥",
        "bulb" | "lightbulb" => "💡",
        "check" | "white_check_mark" => "✅",
        "heavy_check_mark" => "✔",
        "ballot_box" | "unchecked" => "☐",
        "x" | "cross_mark" => "❌",
        "negative_squared_cross_mark" => "❎",
        "warning" => "⚠️",
        "heart" => "❤️",
        "wave" => "👋",
        "tada" | "party" => "🎉",
        "star" => "⭐",
        "thumbsup" | "+1" => "👍",
        "thumbsdown" | "-1" => "👎",
        "computer" | "laptop" => "💻",
        "game" | "video_game" => "🎮",
        "pushpin" => "📌",
        "bar_chart" | "chart" => "📊",
        "smile" | "blush" => "😊",
        "grin" => "😄",
        "joy" => "😂",
        "thinking" => "🤔",
        "100" => "💯",
        "eyes" => "👀",
        "clap" => "👏",
        "muscle" => "💪",
        "crown" => "👑",
        "zap" => "⚡",
        "art" => "🎨",
        "wrench" => "🔧",
        "hammer_and_wrench" => "🛠️",
        "book" => "📖",
        "link" => "🔗",
        "robot" => "🤖",
        "brain" => "🧠",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shortcode_expansion_preserves_code() {
        let inp = "Kirim :rocket: ke `:rocket:` dengan :sparkles: & :tada:!";
        let exp = expand_shortcodes(inp);
        assert_eq!(exp, "Kirim 🚀 ke `:rocket:` dengan ✨ & 🎉!");
    }

    #[test]
    fn test_emoji_row_and_leading_split() {
        assert_eq!(
            extract_pure_emoji_row("🎉 🚀 ✨"),
            Some(vec!["🎉".into(), "🚀".into(), "✨".into()])
        );
        assert_eq!(extract_pure_emoji_row("Halo 👋"), None);
        assert_eq!(extract_pure_emoji_row("1. Langkah"), None);

        let (em, rest) = split_leading_emoji("🚀 Rekomendasi Utama").unwrap();
        assert_eq!(em, "🚀");
        assert_eq!(rest, "Rekomendasi Utama");
        assert!(split_leading_emoji("👨‍💻 Developer Notes")
            .is_some_and(|(e, r)| e == "👨‍💻" && r == "Developer Notes"));
    }

    #[cfg(windows)]
    #[test]
    fn test_direct2d_color_emoji_rasterizes_rgba() {
        for sample in ["🚀", "🎉", "✨", "👨‍💻", "🇮🇩"] {
            let img = render_color_emoji(sample, 40)
                .unwrap_or_else(|| panic!("expected color emoji image for {sample}"));
            assert_eq!((img.size().width, img.size().height), (40, 40));
            let buf = img.to_rgba8().expect("rgba8 buffer");
            let has_opaque = buf.as_slice().iter().any(|px| px.a > 200);
            let has_transparent = buf.as_slice().iter().any(|px| px.a == 0);
            assert!(has_opaque, "{sample} had zero opaque pixels");
            assert!(has_transparent, "{sample} had zero transparent padding");
        }
    }

    #[test]
    fn test_status_badges_render_crisp_green_todo_red() {
        let done = render_status_badge("done", 40);
        let todo = render_status_badge("todo", 40);
        let cross = render_status_badge("cross", 40);
        assert_eq!((done.size().width, done.size().height), (40, 40));
        assert_eq!((todo.size().width, todo.size().height), (40, 40));
        assert_eq!((cross.size().width, cross.size().height), (40, 40));

        let db = done.to_rgba8().unwrap();
        assert!(db.as_slice().iter().any(|p| p.a > 220 && p.g > p.r + 30 && p.g > p.b));
        assert!(db.as_slice().iter().any(|p| p.a > 240 && p.r > 230 && p.g > 230 && p.b > 230));
        assert!(db.as_slice().iter().any(|p| p.a == 0));

        let cb = cross.to_rgba8().unwrap();
        assert!(cb.as_slice().iter().any(|p| p.a > 220 && p.r > p.g + 40 && p.r > p.b + 40));
        assert!(cb.as_slice().iter().any(|p| p.a > 240 && p.r > 230 && p.g > 230 && p.b > 230));
        assert!(cb.as_slice().iter().any(|p| p.a == 0));

        let tb = todo.to_rgba8().unwrap();
        assert!(tb.as_slice().iter().any(|p| p.a > 200 && p.b > p.r));
        assert!(tb.as_slice().iter().any(|p| p.a == 0));
    }
}
