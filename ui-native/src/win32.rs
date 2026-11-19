//! Win32 helpers that the WebView2 frontend got for free from the browser
//! engine, but that a native GUI has to do by hand:
//!
//! * `backdrop-filter: blur(28px)`  ->  DWM acrylic blur-behind
//! * `data-tauri-drag-region`        ->  `WM_NCLBUTTONDOWN` / `HTCAPTION`
//! * `always-on-top`                ->  `SetWindowPos` with `HWND_TOPMOST`
//! * transparent window              ->  `WS_EX_LAYERED` + `DwmExtendFrameIntoClientArea`

use std::ffi::c_void;

use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW,
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE,
};

/// Opens a URL in the user's default browser. Used by the vault's
/// "Buka Link" button, which points at the provider's token page.
pub fn open_url(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
}

/// Finds the largest visible top-level window whose title matches exactly.
/// Used for the borderless drag handle: `FindWindowW` may return a small
/// helper/tooltip window that shares the title, so we pick by area instead.
pub fn find_largest_window_by_title(title: &str) -> Option<*mut c_void> {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowRect, IsWindowVisible,
    };

    struct Ctx {
        title: Vec<u16>,
        best: Option<*mut c_void>,
        best_area: i64,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam as *mut Ctx);
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if len <= 0 {
            return 1;
        }
        if buf[..len as usize] != ctx.title[..ctx.title.len() - 1] {
            return 1;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return 1;
        }
        let area = (rect.right - rect.left) as i64 * (rect.bottom - rect.top) as i64;
        if area > ctx.best_area {
            ctx.best_area = area;
            ctx.best = Some(hwnd);
        }
        1
    }

    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let mut ctx = Ctx {
        title: wide,
        best: None,
        best_area: 0,
    };
    unsafe {
        EnumWindows(Some(callback), &mut ctx as *mut Ctx as LPARAM);
    }
    ctx.best
}

/// Finds a top-level window by its exact title.
pub fn find_window_by_title(title: &str) -> Option<*mut c_void> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

/// Opens the native Windows file picker and returns the selected path.
///
/// The composer's `+` button is the only attach entry point; keeping the
/// dialog native avoids adding a browser/WebView2 dependency just for a file
/// input.
pub fn pick_attachment_file() -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    let mut buffer = [0u16; 1024];
    let mut filter: Vec<u16> = "Semua file\0*.*\0Gambar\0*.png;*.jpg;*.jpeg;*.webp;*.gif\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Pilih file untuk dilampirkan\0".encode_utf16().collect();

    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = find_window_by_title("Mini Assistant HUD")
        .unwrap_or(std::ptr::null_mut()) as windows_sys::Win32::Foundation::HWND;
    ofn.lpstrFilter = filter.as_mut_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buffer.as_mut_ptr();
    ofn.nMaxFile = buffer.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    let picked = unsafe { GetOpenFileNameW(&mut ofn) } != 0;
    if !picked {
        return None;
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// Releases the transient shell pages loaded by the native file picker.
///
/// `GetOpenFileNameW` pulls in `comdlg32`, the shell folder view, and preview
/// handlers. Those shared pages can inflate the HUD's working set long after
/// the dialog is gone, even though the app itself only needs a fraction of
/// them. Trim them once the post-dialog repaint has settled.
pub fn trim_working_set() {
    use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Enables the Windows 11 acrylic backdrop and clips the HUD to its rounded
/// glass silhouette.
///
/// The legacy `SetWindowCompositionAttribute` acrylic path turns Slint's
/// software-rendered surface into an opaque black rectangle on current Windows
/// 11 builds. `DWMWA_SYSTEMBACKDROP_TYPE` is the supported DWM path and keeps
/// the blurred desktop visible behind the translucent Slint fill.
pub fn enable_acrylic(hwnd: *mut c_void, force: bool) -> bool {
    // The HUD opts in by default now that the composer glass is intentionally
    // translucent. Keep the bubble behind the env flag: a rectangular DWM
    // blur would expose a square halo around its circular orb.
    if std::env::var_os("MINI_ACRYLIC_OFF").is_some() {
        return false;
    }
    if !force && std::env::var_os("MINI_ACRYLIC").is_none() {
        return false;
    }

    // Ask DWM to round the top-level window before applying the backdrop.
    // This keeps the acrylic backdrop from showing a square halo outside the
    // Slint glass corners.
    if force {
        let preference = DWMWCP_ROUND;
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &preference as *const _ as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }

    // Default to DWMSBT_NONE (pure, crisp alpha translucency supplied by Slint's
    // glass fill) to avoid heavy Win11 Mica wallpaper occlusion or heavy Acrylic
    // smudge behind text. Opt in to Acrylic via MINI_ACRYLIC_FULL=1.
    let backdrop = if std::env::var_os("MINI_ACRYLIC_FULL").is_some() {
        DWMSBT_TRANSIENTWINDOW
    } else {
        DWMSBT_NONE
    };
    let hr = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &backdrop as *const _ as *const c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    hr >= 0
}

/// Copies plain text to the clipboard (CF_UNICODETEXT). Best effort: a busy
/// Retries `OpenClipboard` briefly to survive transient locks from clipboard
/// viewers or back-to-back writes by the UI toolkit.
unsafe fn open_clipboard_retry() -> bool {
    use windows_sys::Win32::System::DataExchange::OpenClipboard;
    for attempt in 0..8 {
        if OpenClipboard(std::ptr::null_mut()) != 0 {
            return true;
        }
        if attempt < 7 {
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
    }
    false
}

/// clipboard owner or OOM simply leaves the clipboard untouched.
pub fn copy_text(text: &str) {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, SetClipboardData,
    };
    const CF_UNICODETEXT: u32 = 13u32;
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;
    unsafe {
        if !open_clipboard_retry() {
            return;
        }
        EmptyClipboard();
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !handle.is_null() {
            let dst = GlobalLock(handle);
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst as *mut u8, bytes);
                GlobalUnlock(handle);
                SetClipboardData(CF_UNICODETEXT, handle);
            }
        }
        CloseClipboard();
    }
}

/// Toggles the always-on-top flag (the `HWND_TOPMOST` pin button).
pub fn set_always_on_top(hwnd: *mut c_void, on: bool) {
    unsafe {
        SetWindowPos(
            hwnd,
            if on { HWND_TOPMOST } else { HWND_NOTOPMOST },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Monitor edge the floating bubble docked to. Mirrors the four values used by
/// the old webview's `bubbleEdge` state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl ScreenEdge {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// Current desktop cursor position in physical pixels.
pub fn cursor_position() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut p = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}

/// True while the primary mouse button is down. Polled by the bubble drag
/// pump so the Slint event loop (and the hold animation) keeps running.
pub fn left_button_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

    (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0
}

/// Physical window rectangle as `(left, top, width, height)`.
pub fn window_rect(hwnd: *mut c_void) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return None;
    }
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

/// Physical rectangle of the monitor closest to `hwnd`.
fn monitor_rect(hwnd: *mut c_void) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let r = info.rcMonitor;
    Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
}

fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
    if max < min {
        max
    } else {
        value.clamp(min, max)
    }
}

/// Moves the bubble without resizing it, keeping it topmost.
pub fn move_window(hwnd: *mut c_void, x: i32, y: i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    };

    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Snaps the floating bubble to the nearest monitor edge with the same peek
/// ratios as the old webview (25% of the width on left/right, 20% of the height
/// on top/bottom). Tie-break order is bottom, top, right, left.
pub fn snap_window_to_edge(hwnd: *mut c_void) -> Option<ScreenEdge> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    };

    let (wx, wy, ww, wh) = window_rect(hwnd)?;
    let (mx, my, mw, mh) = monitor_rect(hwnd)?;
    if ww <= 0 || wh <= 0 || mw <= 0 || mh <= 0 {
        return None;
    }

    let cx = wx + ww / 2;
    let cy = wy + wh / 2;
    let dl = (wx - mx).abs();
    let dr = (mx + mw - (wx + ww)).abs();
    let dt = (wy - my).abs();
    let db = (my + mh - (wy + wh)).abs();
    let m = dl.min(dr).min(dt).min(db);

    let orb_w = ww.min(wh).max(1);
    let peek_x = ((orb_w as f32) * 0.25).round() as i32;
    let peek_y = ((orb_w as f32) * 0.20).round() as i32;

    let (edge, nx, ny) = if m == db {
        (
            ScreenEdge::Bottom,
            clamp_i32(cx - ww / 2, mx, mx + mw - ww),
            my + mh - wh + peek_y,
        )
    } else if m == dt {
        (
            ScreenEdge::Top,
            clamp_i32(cx - ww / 2, mx, mx + mw - ww),
            my - peek_y,
        )
    } else if m == dr {
        (
            ScreenEdge::Right,
            mx + mw - ww + peek_x,
            clamp_i32(cy - wh / 2, my, my + mh - wh),
        )
    } else {
        (
            ScreenEdge::Left,
            mx - peek_x,
            clamp_i32(cy - wh / 2, my, my + mh - wh),
        )
    };

    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            nx,
            ny,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    Some(edge)
}

/// Shrinks the HUD into the bubble's top-left position (the old webview resized
/// the same window, so the bubble started at the HUD's current x/y before
/// snapping). Both windows are positioned before the bubble is shown.
pub fn position_bubble_from_hud(bubble: *mut c_void, hud: *mut c_void) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    };

    let (hx, hy, _, _) = match window_rect(hud) {
        Some(v) => v,
        None => return,
    };
    let (_, _, bw, bh) = match window_rect(bubble) {
        Some(v) => v,
        None => return,
    };
    let (mx, my, mw, mh) = match monitor_rect(hud) {
        Some(v) => v,
        None => return,
    };
    let nx = clamp_i32(hx, mx + 10, mx + mw - bw - 10);
    let ny = clamp_i32(hy, my + 10, my + mh - bh - 10);

    unsafe {
        SetWindowPos(
            bubble,
            HWND_TOPMOST,
            nx,
            ny,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Re-expands the HUD near the bubble that was clicked. Uses the bubble's
/// current monitor and clamps the 380x620 card inside it, just like the web
/// app's `cmd_set_window_mode("overlay_hud")` path.
pub fn place_hud_from_bubble(hud: *mut c_void, bubble: *mut c_void) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, SWP_NOSIZE,
    };

    let (bx, by, _, _) = match window_rect(bubble) {
        Some(v) => v,
        None => return,
    };
    let (_, _, hw, hh) = match window_rect(hud) {
        Some(v) => v,
        None => return,
    };
    let (mx, my, mw, mh) = match monitor_rect(bubble) {
        Some(v) => v,
        None => return,
    };
    let nx = clamp_i32(bx, mx + 10, mx + mw - hw - 10);
    let ny = clamp_i32(by, my + 10, my + mh - hh - 10);

    unsafe {
        SetWindowPos(
            hud,
            std::ptr::null_mut(),
            nx,
            ny,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Keeps the bubble window cleanly inside the monitor bounds when its size
/// toggles between the 92x92 mascot orb and the expanded translation bubble.
/// When docked on the right edge, keeps the mascot orb anchored on the right
/// so the speech bubble opens inward to the left.
pub fn reposition_bubble_after_resize(
    bubble: *mut c_void,
    prev_rect: Option<(i32, i32, i32, i32)>,
    expanded: bool,
    edge: &str,
    target_w: f32,
    target_h: f32,
    scale: f32,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE,
    };

    let (cur_x, cur_y, cur_w, cur_h) = match window_rect(bubble) {
        Some(v) => v,
        None => return,
    };
    let s = if scale > 0.1 { scale } else { 1.0 };
    let nw = (target_w.max(92.0) * s).round() as i32;
    let nh = (target_h.max(92.0) * s).round() as i32;
    let (px, py, _pw, _ph) = prev_rect.unwrap_or((cur_x, cur_y, cur_w, cur_h));
    let (mx, my, mw, mh) = match monitor_rect(bubble) {
        Some(v) => v,
        None => return,
    };

    let orb_w = (92.0 * s).round() as i32;
    let orb_h = (92.0 * s).round() as i32;
    let orb_peek_x = ((orb_w as f32) * 0.25).round() as i32;
    let orb_peek_y = ((orb_h as f32) * 0.20).round() as i32;

    let nx = match edge {
        "left" => mx - orb_peek_x,
        "right" => mx + mw - nw + orb_peek_x,
        _ => {
            if expanded {
                clamp_i32(px.max(mx + 6), mx + 6, mx + mw - nw - 6)
            } else {
                clamp_i32(px, mx, mx + mw - nw)
            }
        }
    };

    let ny = match edge {
        "top" => my - orb_peek_y,
        "bottom" => my + mh - nh + orb_peek_y,
        _ => {
            if expanded {
                clamp_i32(py.max(my + 6), my + 6, my + mh - nh - 6)
            } else {
                clamp_i32(py, my, my + mh - nh)
            }
        }
    };

    unsafe {
        SetWindowPos(
            bubble,
            HWND_TOPMOST,
            nx,
            ny,
            nw,
            nh,
            SWP_NOACTIVATE,
        );
    }
}

/// Starts a native window drag (the `data-tauri-drag-region` behaviour).
///
/// Implemented with `WM_NCLBUTTONDOWN` / `HTCAPTION` so the OS keeps handling
/// the move, snapping and multi-monitor behaviour for us.
static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn begin_window_drag(hwnd: *mut c_void) {
    use std::sync::atomic::Ordering;
    if DRAGGING.swap(true, Ordering::SeqCst) {
        return;
    }
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    use std::thread;
    use std::time::Duration;

    unsafe {
        // Slint's own window procedure does not honour HTCAPTION for this
        // borderless window, so drive the move ourselves while the left
        // mouse button stays down. This keeps the window following the cursor.
        let mut start_cursor = POINT { x: 0, y: 0 };
        let mut start_rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetCursorPos(&mut start_cursor) == 0 {
            return;
        }
        if GetWindowRect(hwnd, &mut start_rect) == 0 {
            return;
        }

        loop {
            let mut cursor = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut cursor) == 0 {
                break;
            }
            // Stop once the left button is released.
            if (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) == 0 {
                break;
            }
            let new_x = start_rect.left + (cursor.x - start_cursor.x);
            let new_y = start_rect.top + (cursor.y - start_cursor.y);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                new_x,
                new_y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            thread::sleep(Duration::from_millis(16));
        }
        std::sync::atomic::AtomicBool::store(&DRAGGING, false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Reads plain Unicode text (`CF_UNICODETEXT`) from the Windows clipboard.
#[cfg(any(test, feature = "dev-tools"))]
pub fn read_clipboard_text() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    unsafe {
        if !open_clipboard_retry() {
            return None;
        }
        let handle = GetClipboardData(13 /* CF_UNICODETEXT */);
        let result = if !handle.is_null() {
            let ptr = GlobalLock(handle as _) as *const u16;
            if !ptr.is_null() {
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                let s = String::from_utf16_lossy(slice);
                GlobalUnlock(handle as _);
                Some(s)
            } else {
                None
            }
        } else {
            None
        };
        CloseClipboard();
        result
    }
}

/// System-wide physical memory utilization (0..=100) via `GlobalMemoryStatusEx`.
pub fn memory_load_percent() -> Option<u32> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    unsafe {
        let mut stat = std::mem::zeroed::<MEMORYSTATUSEX>();
        stat.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut stat) == 0 {
            return None;
        }
        Some(stat.dwMemoryLoad)
    }
}

// ---------------------------------------------------------------------------
//  Modern Windows Media Foundation + WASAPI 32-bit float streaming audio
//  (dedicated lazy worker thread, 0 extra crates, studio-clean MP3/WAV/M4A)
// ---------------------------------------------------------------------------

fn clean_win_path(path: &std::path::Path) -> String {
    let raw = std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
}

enum AudioCmd {
    Play {
        path: std::path::PathBuf,
        session_id: u64,
    },
    Shutdown,
}

/// Crystal-clear native Windows audio player backed by Media Foundation
/// (`IMFSourceReader` -> 32-bit IEEE float PCM) and WASAPI shared mode
/// (`AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`). Spawns its worker thread lazily
/// on first `play` and calls `MFShutdown()` immediately whenever idle.
pub struct AudioPlayer {
    tx: std::sync::OnceLock<std::sync::mpsc::Sender<AudioCmd>>,
    active_session: std::sync::Arc<std::sync::atomic::AtomicU64>,
    volume_pct: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self {
            tx: std::sync::OnceLock::new(),
            active_session: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            volume_pct: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(85)),
        }
    }

    fn sender(&self) -> &std::sync::mpsc::Sender<AudioCmd> {
        self.tx.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<AudioCmd>();
            let active = self.active_session.clone();
            let vol = self.volume_pct.clone();
            let _ = std::thread::Builder::new()
                .name("aemeath-audio".into())
                .spawn(move || audio_worker(rx, active, vol));
            tx
        })
    }

    pub fn play(&self, path: &std::path::Path, volume_pct: u8, session_id: u64) {
        use std::sync::atomic::Ordering;
        self.volume_pct.store(volume_pct.min(100), Ordering::Relaxed);
        self.active_session.store(session_id, Ordering::Release);
        if self
            .sender()
            .send(AudioCmd::Play {
                path: path.to_path_buf(),
                session_id,
            })
            .is_err()
        {
            self.active_session.store(0, Ordering::Release);
        }
    }

    pub fn set_volume(&self, volume_pct: u8) {
        self.volume_pct
            .store(volume_pct.min(100), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.active_session
            .store(0, std::sync::atomic::Ordering::Release);
    }

    /// Current playing session id, or `0` when stopped / finished naturally.
    pub fn active_session_id(&self) -> u64 {
        self.active_session.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Silent Media Foundation decode + duration probe used by unit tests.
    #[cfg(test)]
    pub fn probe_track_ms(path: &std::path::Path) -> Result<u64, String> {
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::{
            MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFShutdown,
            MFStartup, MFAudioFormat_Float, MFSTARTUP_LITE, MF_MT_AUDIO_NUM_CHANNELS,
            MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
            MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_MEDIASOURCE, MF_VERSION,
        };
        use windows::Win32::System::Com::StructuredStorage::PropVariantToUInt64;
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

        unsafe {
            let co_inited = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let result = (|| -> Result<u64, String> {
                MFStartup(MF_VERSION, MFSTARTUP_LITE)
                    .map_err(|e| format!("MFStartup failed: {e}"))?;
                struct MfDrop;
                impl Drop for MfDrop {
                    fn drop(&mut self) {
                        unsafe {
                            let _ = MFShutdown();
                        }
                    }
                }
                let _mf = MfDrop;

                let url = HSTRING::from(clean_win_path(path));
                let reader = MFCreateSourceReaderFromURL(&url, None)
                    .map_err(|e| format!("MFCreateSourceReaderFromURL failed: {e}"))?;

                let stream_idx = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
                let partial = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
                partial
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                    .map_err(|e| format!("SetGUID major: {e}"))?;
                partial
                    .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
                    .map_err(|e| format!("SetGUID float: {e}"))?;
                reader
                    .SetCurrentMediaType(stream_idx, None, &partial)
                    .map_err(|e| format!("SetCurrentMediaType float: {e}"))?;

                let cur = reader
                    .GetCurrentMediaType(stream_idx)
                    .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
                let ch = cur
                    .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                    .map_err(|e| format!("channels: {e}"))?;
                let sr = cur
                    .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                    .map_err(|e| format!("sample_rate: {e}"))?;
                if ch == 0 || sr == 0 {
                    return Err(format!("invalid audio stream params ch={ch} sr={sr}"));
                }

                // Verify real 32-bit float sample decoding succeeds.
                let mut flags = 0u32;
                let mut sample = None;
                reader
                    .ReadSample(stream_idx, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .map_err(|e| format!("ReadSample: {e}"))?;
                if sample.is_none() {
                    return Err("ReadSample returned no audio sample".into());
                }

                let pv = reader
                    .GetPresentationAttribute(
                        MF_SOURCE_READER_MEDIASOURCE.0 as u32,
                        &MF_PD_DURATION,
                    )
                    .map_err(|e| format!("GetPresentationAttribute duration: {e}"))?;
                let hns = PropVariantToUInt64(&pv)
                    .map_err(|e| format!("PropVariantToUInt64: {e}"))?;
                Ok(hns / 10_000)
            })();
            if co_inited {
                CoUninitialize();
            }
            result
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop();
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(AudioCmd::Shutdown);
        }
    }
}

fn audio_worker(
    rx: std::sync::mpsc::Receiver<AudioCmd>,
    active_session: std::sync::Arc<std::sync::atomic::AtomicU64>,
    volume_pct: std::sync::Arc<std::sync::atomic::AtomicU8>,
) {
    use std::sync::atomic::Ordering;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    let co_inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            AudioCmd::Play { path, session_id } => {
                if active_session.load(Ordering::Acquire) != session_id {
                    continue;
                }
                if let Err(_err) = play_mf_wasapi(&path, session_id, &active_session, &volume_pct) {
                    #[cfg(debug_assertions)]
                    eprintln!("[audio] playback error: {_err}");
                }
                let _ = active_session.compare_exchange(
                    session_id,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            AudioCmd::Shutdown => {
                active_session.store(0, Ordering::Release);
                break;
            }
        }
    }

    if co_inited {
        unsafe {
            CoUninitialize();
        }
    }
}

fn play_mf_wasapi(
    path: &std::path::Path,
    session_id: u64,
    active_session: &std::sync::atomic::AtomicU64,
    volume_pct: &std::sync::atomic::AtomicU8,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use windows::core::HSTRING;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    };
    use windows::Win32::Media::MediaFoundation::{
        MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFShutdown, MFStartup,
        MFAudioFormat_Float, MFSTARTUP_LITE, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
        MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ALL_STREAMS,
        MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_VERSION,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| format!("MFStartup: {e}"))?;
        struct MfGuard;
        impl Drop for MfGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = MFShutdown();
                }
            }
        }
        let _mf = MfGuard;

        let url = HSTRING::from(clean_win_path(path));
        let reader = MFCreateSourceReaderFromURL(&url, None)
            .map_err(|e| format!("MFCreateSourceReaderFromURL: {e}"))?;

        let stream_idx = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        reader
            .SetStreamSelection(stream_idx, true)
            .map_err(|e| format!("SetStreamSelection: {e}"))?;

        let out_type = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| format!("SetGUID major: {e}"))?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
            .map_err(|e| format!("SetGUID float: {e}"))?;
        reader
            .SetCurrentMediaType(stream_idx, None, &out_type)
            .map_err(|e| format!("SetCurrentMediaType: {e}"))?;

        let cur_type = reader
            .GetCurrentMediaType(stream_idx)
            .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
        let channels = cur_type
            .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
            .map_err(|e| format!("channels: {e}"))? as u16;
        let sample_rate = cur_type
            .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
            .map_err(|e| format!("sample_rate: {e}"))?;
        if channels == 0 || sample_rate == 0 {
            return Err(format!("invalid stream format ch={channels} sr={sample_rate}"));
        }

        let block_align = channels.saturating_mul(4);
        let wfx = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate.saturating_mul(block_align as u32),
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
        let audio_client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("Activate IAudioClient: {e}"))?;

        // 120 ms shared-mode buffer with high-quality WASAPI format/rate conversion.
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        audio_client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_200_000, 0, &wfx, None)
            .map_err(|e| format!("IAudioClient::Initialize: {e}"))?;

        let buffer_frames = audio_client
            .GetBufferSize()
            .map_err(|e| format!("GetBufferSize: {e}"))?;
        let render_client: IAudioRenderClient = audio_client
            .GetService()
            .map_err(|e| format!("GetService IAudioRenderClient: {e}"))?;

        audio_client
            .Start()
            .map_err(|e| format!("IAudioClient::Start: {e}"))?;

        let ch_usize = channels as usize;
        let fade_in_frames = (sample_rate / 65).max(1) as u64; // ~15 ms anti-pop ramp
        let mut frames_written_total: u64 = 0;
        let mut pending: Vec<f32> = Vec::new();
        let mut pending_frame_pos: usize = 0;
        let mut eof = false;

        loop {
            if active_session.load(Ordering::Acquire) != session_id {
                break;
            }

            let pending_frames_total = pending.len() / ch_usize;
            if pending_frame_pos >= pending_frames_total && !eof {
                pending.clear();
                pending_frame_pos = 0;
                let mut stream_flags = 0u32;
                let mut sample_opt = None;
                reader
                    .ReadSample(
                        stream_idx,
                        0,
                        None,
                        Some(&mut stream_flags),
                        None,
                        Some(&mut sample_opt),
                    )
                    .map_err(|e| format!("ReadSample: {e}"))?;
                if (stream_flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    eof = true;
                }
                if let Some(sample) = sample_opt {
                    let buf = sample
                        .ConvertToContiguousBuffer()
                        .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
                    let mut ptr = std::ptr::null_mut();
                    let mut cur_len = 0u32;
                    buf.Lock(&mut ptr, None, Some(&mut cur_len))
                        .map_err(|e| format!("IMFMediaBuffer::Lock: {e}"))?;
                    let bytes = std::slice::from_raw_parts(ptr, cur_len as usize);
                    pending.reserve(bytes.len() / 4);
                    for chunk in bytes.chunks_exact(4) {
                        pending.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                    }
                    let _ = buf.Unlock();
                }
            }

            let pending_frames_left = (pending.len() / ch_usize).saturating_sub(pending_frame_pos);
            if pending_frames_left == 0 {
                if eof {
                    // Drain remaining queued frames cleanly so the track ending isn't cut off.
                    while active_session.load(Ordering::Acquire) == session_id {
                        let pad = audio_client.GetCurrentPadding().unwrap_or(0);
                        if pad == 0 {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(12));
                    }
                    break;
                }
                continue;
            }

            let padding = audio_client
                .GetCurrentPadding()
                .map_err(|e| format!("GetCurrentPadding: {e}"))?;
            let avail = buffer_frames.saturating_sub(padding) as usize;
            if avail == 0 {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let to_write = avail.min(pending_frames_left);
            let dst_ptr = render_client
                .GetBuffer(to_write as u32)
                .map_err(|e| format!("GetBuffer: {e}"))?;
            let dst_bytes = std::slice::from_raw_parts_mut(dst_ptr, to_write * ch_usize * 4);
            let vol = (volume_pct.load(Ordering::Relaxed).min(100) as f32) / 100.0;

            for frame_i in 0..to_write {
                let ramp = if frames_written_total < fade_in_frames {
                    frames_written_total as f32 / fade_in_frames as f32
                } else {
                    1.0
                };
                let gain = vol * ramp;
                let src_base = (pending_frame_pos + frame_i) * ch_usize;
                let dst_base = frame_i * ch_usize * 4;
                for c in 0..ch_usize {
                    let s = (pending[src_base + c] * gain).clamp(-0.98, 0.98);
                    let b = s.to_le_bytes();
                    let o = dst_base + c * 4;
                    dst_bytes[o..o + 4].copy_from_slice(&b);
                }
                frames_written_total += 1;
            }

            render_client
                .ReleaseBuffer(to_write as u32, 0)
                .map_err(|e| format!("ReleaseBuffer: {e}"))?;
            pending_frame_pos += to_write;
        }

        let _ = audio_client.Stop();
        Ok(())
    }
}

/// Holds a process-lifetime named Windows mutex so double-clicking the app
/// while it is already open focuses the existing window instead of spawning a
/// duplicate instance or colliding on port 8787.
pub struct SingleInstanceGuard(*mut c_void);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

/// Attempts to acquire a named Win32 mutex. Returns `None` if another instance
/// already owns the mutex (`ERROR_ALREADY_EXISTS`).
pub fn acquire_single_instance(name: &str) -> Option<SingleInstanceGuard> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
        if handle.is_null() {
            return Some(SingleInstanceGuard(std::ptr::null_mut()));
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return None;
        }
        Some(SingleInstanceGuard(handle))
    }
}

/// Brings an already-running Mini Assistant window to the foreground.
pub fn focus_existing_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };

    for title in ["Mini Assistant HUD", "Mini Assistant", "Mini Assistant Bubble"] {
        if let Some(hwnd) = find_largest_window_by_title(title)
            .or_else(|| find_window_by_title(title))
        {
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            }
            return;
        }
    }
}

/// Attaches stdout/stderr to the launching terminal when running CLI QA flags
/// on a `windows_subsystem = "windows"` release build.
#[cfg(feature = "dev-tools")]
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
