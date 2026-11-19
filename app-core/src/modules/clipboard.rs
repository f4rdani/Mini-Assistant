//! Windows clipboard reader for screenshots.
//!
//! Win+Shift+S (and the PrtScrn variants) put a CF_DIB on the clipboard. The
//! HUD polls the sequence number and, when the composer has focus, turns the
//! newest bitmap into an attachment — the Ctrl+V flow without fighting the
//! TextInput for the key event.

#![allow(clippy::missing_safety_doc)]

use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
};
const CF_DIB: u32 = 8u32;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

/// Monotonic clipboard counter; 0 means the call failed.
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Newest bitmap on the clipboard, encoded as PNG bytes. None when the
/// clipboard holds no DIB or the format is one we cannot decode.
pub fn image_png() -> Option<Vec<u8>> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let out = (|| {
            let handle = GetClipboardData(CF_DIB);
            if handle.is_null() {
                return None;
            }
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                return None;
            }
            let size = GlobalSize(handle);
            let dib = std::slice::from_raw_parts(ptr as *const u8, size);
            let rgba = dib_to_rgba(dib);
            GlobalUnlock(handle);
            rgba
        })();
        CloseClipboard();
        out
    }
}

/// Minimal BITMAPINFOHEADER decoder: 24/32bpp uncompressed, bottom-up or
/// top-down. Returns PNG bytes.
fn dib_to_rgba(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let u16le = |o: usize| u16::from_le_bytes([dib[o], dib[o + 1]]);
    let i32le = |o: usize| i32::from_le_bytes([dib[o], dib[o + 1], dib[o + 2], dib[o + 3]]);
    let header = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let width = i32le(4).max(0) as usize;
    let height_raw = i32le(8);
    let top_down = height_raw < 0;
    let height = height_raw.unsigned_abs() as usize;
    let bpp = u16le(14) as usize;
    if width == 0 || height == 0 || (bpp != 24 && bpp != 32) {
        return None;
    }
    let row = (width * bpp).div_ceil(32) * 4; // padded to 4 bytes
    let pixel = |x: usize, y: usize| -> [u8; 3] {
        let src_y = if top_down { y } else { height - 1 - y };
        let o = header + src_y * row + x * (bpp / 8);
        if o + 2 < dib.len() { [dib[o], dib[o + 1], dib[o + 2]] } else { [0, 0, 0] }
    };
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let [b, g, r] = pixel(x, y);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    let mut png = Vec::new();
    let enc = image::codecs::png::PngEncoder::new(std::io::Cursor::new(&mut png));
    image::ImageEncoder::write_image(
        enc,
        &rgba,
        width as u32,
        height as u32,
        image::ExtendedColorType::Rgba8,
    )
    .ok()?;
    Some(png)
}