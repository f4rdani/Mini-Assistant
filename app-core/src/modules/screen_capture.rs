use base64::prelude::*;
use image::{ImageBuffer, Rgb};
use std::io::Cursor;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetDeviceCaps, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DESKTOPHORZRES,
    DESKTOPVERTRES, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub fn capture_screen_base64() -> Result<String, String> {
    capture_region_base64(None)
}

/// Tangkap layar spesifik window target (game/aplikasi), atau fallback ke desktop
pub fn capture_target_window_or_screen_base64(
    assistant_hwnd: Option<isize>,
) -> (Result<String, String>, Option<crate::modules::window_detector::CurrentWindowInfo>) {
    let target = crate::modules::window_detector::detect_target_window(assistant_hwnd.map(|h| h as _));
    if let Some(ref win) = target {
        if let Some(bounds) = crate::modules::window_detector::get_window_bounds(win.hwnd as _) {
            let res = capture_region_base64(Some(bounds));
            if res.is_ok() {
                return (res, target);
            }
        }
    }
    (capture_screen_base64(), target)
}

pub fn capture_region_base64(target_region: Option<(i32, i32, i32, i32)>) -> Result<String, String> {
    unsafe {
        let hwnd = GetDesktopWindow();
        let hdc_screen = GetDC(hwnd);
        if hdc_screen.is_null() {
            return Err("Failed to get desktop DC".to_string());
        }

        // Query physical desktop resolution (DPI-independent)
        let desk_w = GetDeviceCaps(hdc_screen, DESKTOPHORZRES as i32);
        let desk_h = GetDeviceCaps(hdc_screen, DESKTOPVERTRES as i32);

        let (x, y, mut width, mut height) = if let Some((rx, ry, rw, rh)) = target_region {
            (rx, ry, rw, rh)
        } else if desk_w > 0 && desk_h > 0 {
            (0, 0, desk_w, desk_h)
        } else {
            let mut vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let mut vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let mut vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let mut vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

            if vw <= 0 || vh <= 0 {
                vx = 0;
                vy = 0;
                vw = GetSystemMetrics(SM_CXSCREEN);
                vh = GetSystemMetrics(SM_CYSCREEN);
            }
            (vx, vy, vw, vh)
        };

        if width < 10 { width = 10; }
        if height < 10 { height = 10; }

        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_null() {
            ReleaseDC(hwnd, hdc_screen);
            return Err("Failed to create compatible DC".to_string());
        }

        let hbitmap = CreateCompatibleBitmap(hdc_screen, width, height);
        if hbitmap.is_null() {
            DeleteDC(hdc_mem);
            ReleaseDC(hwnd, hdc_screen);
            return Err("Failed to create compatible bitmap".to_string());
        }

        let old_bitmap = SelectObject(hdc_mem, hbitmap);

        // BitBlt from screen to memory DC
        let blt_res = BitBlt(hdc_mem, 0, 0, width, height, hdc_screen, x, y, SRCCOPY);
        if blt_res == 0 {
            SelectObject(hdc_mem, old_bitmap);
            DeleteObject(hbitmap);
            DeleteDC(hdc_mem);
            ReleaseDC(hwnd, hdc_screen);
            return Err("Failed to BitBlt screen bitmap".to_string());
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative for top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }; 1],
        };

        let mut bgra_buf: Vec<u8> = vec![0u8; (width * height * 4) as usize];
        let lines = GetDIBits(
            hdc_mem,
            hbitmap,
            0,
            height as u32,
            bgra_buf.as_mut_ptr() as *mut _,
            &mut bmi,
            DIB_RGB_COLORS,
        );

        // Cleanup GDI objects
        SelectObject(hdc_mem, old_bitmap);
        DeleteObject(hbitmap);
        DeleteDC(hdc_mem);
        ReleaseDC(hwnd, hdc_screen);

        if lines == 0 {
            return Err("Failed to extract DIBits from screen".to_string());
        }

        // Convert BGRA to RGB ImageBuffer
        let mut rgb_buf: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::new(width as u32, height as u32);

        for (i, pixel) in rgb_buf.pixels_mut().enumerate() {
            let offset = i * 4;
            if offset + 2 < bgra_buf.len() {
                let b = bgra_buf[offset];
                let g = bgra_buf[offset + 1];
                let r = bgra_buf[offset + 2];
                *pixel = Rgb([r, g, b]);
            }
        }

        // Encode to JPEG in-memory
        let mut jpeg_bytes = Vec::new();
        let mut cursor = Cursor::new(&mut jpeg_bytes);
        rgb_buf
            .write_to(&mut cursor, image::ImageFormat::Jpeg)
            .map_err(|e| format!("Failed to encode JPEG: {}", e))?;

        // Save a copy to %TEMP%/mini_assistant_cache for caching and system access
        let temp_dir = std::env::temp_dir().join("mini_assistant_cache");
        if std::fs::create_dir_all(&temp_dir).is_ok() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let temp_file = temp_dir.join(format!("screenshot_{}.jpg", timestamp));
            let _ = std::fs::write(&temp_file, &jpeg_bytes);

            // Auto-prune: Keep only the latest 12 screenshots (FIFO) to prevent disk clutter
            prune_old_screenshots(&temp_dir, 12);
        }

        let encoded = BASE64_STANDARD.encode(&jpeg_bytes);
        Ok(format!("data:image/jpeg;base64,{}", encoded))
    }
}

fn prune_old_screenshots(dir: &std::path::Path, max_keep: usize) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().map(|e| e == "jpg").unwrap_or(false) {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        files.push((modified, path));
                    }
                }
            }
        }
        // Sort descending by modification time (newest first)
        files.sort_by(|a, b| b.0.cmp(&a.0));
        // Prune older files exceeding threshold
        for (_, path) in files.iter().skip(max_keep) {
            let _ = std::fs::remove_file(path);
        }
    }
}
