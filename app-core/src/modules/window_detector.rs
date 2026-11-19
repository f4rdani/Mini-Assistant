use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetDesktopWindow, GetForegroundWindow, GetShellWindow,
    GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE,
    WS_EX_TOOLWINDOW,
};

static LAST_TARGET_HWND: AtomicIsize = AtomicIsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentWindowInfo {
    pub title: String,
    pub process_name: String,
    pub process_path: String,
    pub suggested_topic: String,
    pub category: String, // "Gaming" | "Development" | "Design" | "Study" | "General"
    pub hwnd: usize,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Catat HWND window pengguna tepat sebelum jendela Mini Assistant mengambil fokus (misal saat hotkey ditekan).
pub fn record_foreground_window() {
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_null() && is_valid_target_window(fg, None) {
            LAST_TARGET_HWND.store(fg as isize, Ordering::SeqCst);
        }
    }
}

pub fn get_recorded_target_hwnd() -> Option<HWND> {
    let val = LAST_TARGET_HWND.load(Ordering::SeqCst);
    if val != 0 {
        let hwnd = val as HWND;
        if is_valid_target_window(hwnd, None) {
            return Some(hwnd);
        }
    }
    None
}

/// Background thread untuk terus memantau window yang sedang aktif digunakan pengguna.
/// Berjalan ringan (setiap 350ms) dan otomatis mencatat window non-Mini-Assistant terakhir.
pub fn start_window_tracker() {
    std::thread::spawn(|| {
        let my_pid = unsafe { GetCurrentProcessId() };
        loop {
            std::thread::sleep(std::time::Duration::from_millis(350));
            unsafe {
                let fg = GetForegroundWindow();
                if !fg.is_null() {
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(fg, &mut pid);
                    if pid != my_pid && pid != 0 && is_valid_target_window(fg, None) {
                        LAST_TARGET_HWND.store(fg as isize, Ordering::SeqCst);
                    }
                }
            }
        }
    });
}

struct EnumZOrderContext {
    my_pid: u32,
    assistant_hwnd: Option<HWND>,
    found_hwnd: Option<HWND>,
}

unsafe extern "system" fn enum_zorder_proc(hwnd: HWND, lparam: isize) -> i32 {
    unsafe {
        let ctx = &mut *(lparam as *mut EnumZOrderContext);
        if is_valid_target_window(hwnd, ctx.assistant_hwnd) {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != ctx.my_pid && pid != 0 {
                ctx.found_hwnd = Some(hwnd);
                return 0; // Berhenti di window aplikasi teratas di Z-order
            }
        }
        1 // Lanjut telusuri window berikutnya
    }
}

pub fn get_top_zorder_target_window(assistant_hwnd: Option<HWND>) -> Option<HWND> {
    unsafe {
        let my_pid = GetCurrentProcessId();
        let mut ctx = EnumZOrderContext {
            my_pid,
            assistant_hwnd,
            found_hwnd: None,
        };
        EnumWindows(Some(enum_zorder_proc), &mut ctx as *mut _ as isize);
        ctx.found_hwnd
    }
}

/// Cari window aktif target (di luar Mini Assistant).
pub fn detect_target_window(assistant_hwnd: Option<HWND>) -> Option<CurrentWindowInfo> {
    unsafe {
        let my_pid = GetCurrentProcessId();

        // 1. Cek window terdepan saat ini jika bukan Mini Assistant
        let fg = GetForegroundWindow();
        if !fg.is_null() && is_valid_target_window(fg, assistant_hwnd) {
            let mut pid = 0u32;
            GetWindowThreadProcessId(fg, &mut pid);
            if pid != my_pid && pid != 0 {
                LAST_TARGET_HWND.store(fg as isize, Ordering::SeqCst);
                return inspect_window(fg);
            }
        }

        // 2. Jika fokus saat ini berada di Mini Assistant, cari window teratas di Z-order
        // (yaitu window yang tepat berada di bawah Mini Assistant) via EnumWindows
        if let Some(top_z) = get_top_zorder_target_window(assistant_hwnd) {
            LAST_TARGET_HWND.store(top_z as isize, Ordering::SeqCst);
            return inspect_window(top_z);
        }

        // 3. Fallback: gunakan window yang terekam pada background tracker
        if let Some(recorded) = get_recorded_target_hwnd() {
            let mut pid = 0u32;
            GetWindowThreadProcessId(recorded, &mut pid);
            if pid != my_pid && pid != 0 {
                return inspect_window(recorded);
            }
        }

        None
    }
}

/// HWND is an opaque OS handle, not a Rust pointer we dereference ourselves.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn get_window_bounds(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut rect as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );

        if (hr != 0 || rect.right <= rect.left || rect.bottom <= rect.top)
            && GetWindowRect(hwnd, &mut rect) == 0
        {
            return None;
        }

        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w > 40 && h > 40 {
            Some((rect.left, rect.top, w, h))
        } else {
            None
        }
    }
}

fn is_valid_target_window(hwnd: HWND, assistant_hwnd: Option<HWND>) -> bool {
    if hwnd.is_null() {
        return false;
    }
    if let Some(a_hwnd) = assistant_hwnd {
        if hwnd == a_hwnd {
            return false;
        }
    }

    unsafe {
        if IsWindow(hwnd) == 0 {
            return false;
        }

        let my_pid = GetCurrentProcessId();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == my_pid || pid == 0 {
            return false;
        }

        let desktop = GetDesktopWindow();
        let shell = GetShellWindow();
        if hwnd == desktop || hwnd == shell {
            return false;
        }

        if IsWindowVisible(hwnd) == 0 {
            return false;
        }
        if IsIconic(hwnd) != 0 {
            return false; // Jangan targetkan window yang di-minimize
        }

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex_style & WS_EX_TOOLWINDOW) != 0 {
            return false;
        }

        // Periksa ukuran window minimal
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) != 0 {
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w < 160 || h < 160 {
                return false;
            }
        } else {
            return false;
        }

        let title_len = GetWindowTextLengthW(hwnd);
        if title_len <= 0 {
            return false;
        }

        let mut buf = vec![0u16; (title_len + 1) as usize];
        let read_len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        let title = String::from_utf16_lossy(&buf[..read_len as usize]).trim().to_string();

        if title.is_empty() {
            return false;
        }

        // Filter window class sistem Windows
        let mut class_buf = [0u16; 128];
        let class_len = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
        if class_len > 0 {
            let class_name = String::from_utf16_lossy(&class_buf[..class_len as usize]);
            let lower_class = class_name.to_lowercase();
            if lower_class == "progman"
                || lower_class == "workerw"
                || lower_class == "shell_traywnd"
                || lower_class == "shell_secondarytraywnd"
                || lower_class == "windows.ui.core.corewindow"
            {
                return false;
            }
        }

        // Abaikan window sistem Windows
        let ignored = [
            "Program Manager",
            "Windows Input Experience",
            "Settings",
            "Task Switching",
            "Windows Shell Experience Host",
            "Mini Assistant",
        ];
        for ig in ignored {
            if title.eq_ignore_ascii_case(ig) {
                return false;
            }
        }

        true
    }
}

fn inspect_window(hwnd: HWND) -> Option<CurrentWindowInfo> {
    unsafe {
        let title_len = GetWindowTextLengthW(hwnd);
        let title = if title_len > 0 {
            let mut buf = vec![0u16; (title_len + 1) as usize];
            let read = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            String::from_utf16_lossy(&buf[..read as usize]).trim().to_string()
        } else {
            String::new()
        };

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);

        let (process_name, process_path) = get_process_info(pid);

        let (x, y, width, height) = get_window_bounds(hwnd).unwrap_or((0, 0, 800, 600));

        let (category, suggested_topic) = classify_window(&title, &process_name, &process_path, width, height);

        Some(CurrentWindowInfo {
            title,
            process_name,
            process_path,
            suggested_topic,
            category,
            hwnd: hwnd as usize,
            x,
            y,
            width,
            height,
        })
    }
}

fn get_process_info(pid: u32) -> (String, String) {
    if pid == 0 {
        return ("Unknown".to_string(), String::new());
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return ("Unknown".to_string(), String::new());
        }

        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let success = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if success != 0 && size > 0 {
            let full_path = String::from_utf16_lossy(&buf[..size as usize]);
            let filename = Path::new(&full_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
                .to_string();
            (filename, full_path)
        } else {
            ("Unknown".to_string(), String::new())
        }
    }
}

/// Klasifikasikan kategori dan buat saran topik ramah pengguna
fn classify_window(
    title: &str,
    process_name: &str,
    process_path: &str,
    width: i32,
    height: i32,
) -> (String, String) {
    let title_lower = title.to_lowercase();
    let proc_lower = process_name.to_lowercase();
    let path_lower = process_path.to_lowercase();

    let clean_title = extract_clean_app_title(title, process_name);

    // 1. Deteksi Browser / Web
    let is_browser = [
        "chrome", "msedge", "firefox", "brave", "opera", "vivaldi"
    ]
    .iter()
    .any(|k| proc_lower.contains(k));

    if is_browser {
        let topic_name = clean_browser_title(title, process_name);
        return ("General".to_string(), format!("[Web] {}", topic_name));
    }

    // 2. Deteksi IDE & Terminal / Development
    let is_dev = [
        "code", "visual studio", "devenv", "intellij", "idea64", "pycharm",
        "webstorm", "sublime", "notepad++", "terminal", "windowsterminal",
        "powershell", "cmd.exe", "conhost", "git", "cursor", "antigravity",
        "clion", "rider", "eclipse", "neovim"
    ]
    .iter()
    .any(|k| title_lower.contains(k) || proc_lower.contains(k));

    if is_dev {
        let dev_title = if proc_lower.contains("windowsterminal") {
            if clean_title.is_empty() || clean_title.eq_ignore_ascii_case("windowsterminal") {
                "Terminal".to_string()
            } else {
                clean_title.clone()
            }
        } else if proc_lower.contains("powershell") {
            "PowerShell".to_string()
        } else if proc_lower.contains("cmd") {
            "Command Prompt".to_string()
        } else {
            clean_title.clone()
        };
        return ("Development".to_string(), format!("[Dev] {}", dev_title));
    }

    // 3. Deteksi Game (Gaming)
    let is_game_path = path_lower.contains("\\steamapps\\common\\")
        || path_lower.contains("\\games\\")
        || path_lower.contains("\\game\\")
        || path_lower.contains("\\riot games\\")
        || path_lower.contains("\\epic games\\")
        || path_lower.contains("\\gog galaxy\\")
        || path_lower.contains("\\ubisoft\\")
        || path_lower.contains("\\ea games\\")
        || path_lower.contains("\\battle.net\\")
        || path_lower.contains("\\roblox\\");

    let is_game_keyword = [
        "genshin", "starrail", "honkai", "dota", "valorant", "cs2", "csgo",
        "minecraft", "roblox", "cyberpunk", "witcher", "wuthering", "apex",
        "league", "fortnite", "overwatch", "pubg", "gta", "elden", "sekiro",
        "rdr2", "sims", "monster hunter", "palworld", "destiny", "warframe",
        "osu!", "osu", "ffxiv", "tarkov", "unity", "unreal", "emulator",
        "yuzu", "ryujinx", "rpcs3", "gameloop", "game", "steam"
    ]
    .iter()
    .any(|k| title_lower.contains(k) || proc_lower.contains(k));

    let is_large_screen = width >= 1280 && height >= 720;
    let is_not_standard_office = !proc_lower.contains("word")
        && !proc_lower.contains("excel")
        && !proc_lower.contains("powerpnt")
        && !proc_lower.contains("explorer");

    if is_game_path || is_game_keyword || (is_large_screen && is_not_standard_office && !title.is_empty()) {
        return ("Gaming".to_string(), format!("[Game] {}", clean_title));
    }

    // 4. Deteksi Design & Multimedia (Design)
    let is_design = [
        "photoshop", "illustrator", "figma", "blender", "canva", "premiere",
        "afterfx", "after effects", "davinci", "resolve", "aseprite", "inkscape",
        "coreldraw", "lightroom", "maya", "3ds max"
    ]
    .iter()
    .any(|k| title_lower.contains(k) || proc_lower.contains(k));

    if is_design {
        return ("Design".to_string(), format!("[Design] {}", clean_title));
    }

    // 5. Deteksi Produktivitas & Belajar (Study)
    let is_study = [
        "word", "excel", "powerpnt", "powerpoint", "notion", "obsidian",
        "acrobat", "foxit", "onenote", "libreoffice"
    ]
    .iter()
    .any(|k| title_lower.contains(k) || proc_lower.contains(k));

    if is_study {
        return ("Study".to_string(), format!("[Study] {}", clean_title));
    }

    // 6. File Explorer
    if proc_lower.contains("explorer") {
        let exp_title = if clean_title.is_empty() || clean_title.eq_ignore_ascii_case("explorer") {
            "File Explorer".to_string()
        } else {
            clean_title.clone()
        };
        return ("General".to_string(), format!("[App] {}", exp_title));
    }

    // Default
    ("General".to_string(), format!("[App] {}", clean_title))
}

fn extract_clean_app_title(title: &str, process_name: &str) -> String {
    let mut t = title.trim();
    if t.is_empty() {
        return process_name.replace(".exe", "");
    }

    // Edge ships "Microsoft​ Edge" in its title: the zero-width space is
    // invisible but makes every suffix comparison below fail, which is how
    // "YouTube: Microsoft Edge" ended up as a session name.
    let cleaned: String = t
        .chars()
        .filter(|c| !matches!(*c, '​' | '‌' | '‍' | '﻿'))
        .collect();
    t = cleaned.trim();

    // VS Code and friends prefix the title with a braille spinner while they
    // work, and end long names with an ellipsis. Both would land in the
    // session name, so they are dropped before anything else looks at it.
    t = t
        .trim_start_matches(|c: char| {
            !c.is_alphanumeric() && !matches!(c, '[' | '(' | '"' | '\'' | '#')
        })
        .trim();
    t = t.trim_end_matches("...").trim_end_matches('…').trim();

    // A console host's title is the command line, so it can be a full path to
    // the executable. That is not a conversation name; use the process.
    let lower_t = t.to_ascii_lowercase();
    if lower_t.ends_with(".exe") && (t.contains('\\') || t.contains('/')) {
        return process_name.replace(".exe", "");
    }

    // Hapus suffix nama browser umum: " - Google Chrome", " - Microsoft Edge", dll.
    let browser_suffixes = [
        " - Google Chrome",
        " - Microsoft Edge",
        " - Mozilla Firefox",
        " - Brave",
        " - Opera",
        " - Vivaldi",
    ];
    for sfx in &browser_suffixes {
        if let Some(stripped) = t.strip_suffix(sfx) {
            t = stripped.trim();
            break;
        }
    }

    // Hapus suffix " - YouTube" jika ada di akhir
    if let Some(stripped) = t.strip_suffix(" - YouTube") {
        t = stripped.trim();
    }

    // Jika formatnya "Halaman - Aplikasi"
    if let Some((first, second)) = t.rsplit_once(" - ") {
        let trimmed_second = second.trim();
        let trimmed_first = first.trim();
        if trimmed_second.len() >= 3 && !trimmed_second.eq_ignore_ascii_case("google chrome") {
            return trimmed_second.to_string();
        } else if !trimmed_first.is_empty() {
            return trimmed_first.to_string();
        }
    }

    if let Some((first, _)) = t.split_once(" | ") {
        let trimmed = first.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // Char-aware, not byte-aware: a title with an accent or an emoji at the cut
    // point would panic on the &str index (this runs on the UI thread every
    // second while the window watcher is alive).
    truncate_topic(t, 30)
}

pub fn clean_browser_title(title: &str, process_name: &str) -> String {
    let mut t = title.trim();
    if t.is_empty() {
        return process_name.replace(".exe", "");
    }

    // Filter zero-width characters (Edge puts zero-width space in "Microsoft Edge")
    let cleaned: String = t
        .chars()
        .filter(|c| !matches!(*c, '​' | '‌' | '‍' | '﻿'))
        .collect();
    t = cleaned.trim();

    // Strip common browser name suffixes
    let browser_suffixes = [
        " - Google Chrome",
        " - Microsoft Edge",
        " - Mozilla Firefox",
        " — Mozilla Firefox",
        " - Brave",
        " - Opera",
        " - Vivaldi",
    ];
    for sfx in &browser_suffixes {
        if let Some(stripped) = t.strip_suffix(sfx) {
            t = stripped.trim();
            break;
        }
    }

    // Strip common browser profile suffixes (e.g. " - Personal", " - Work")
    let profile_suffixes = [
        " - Personal",
        " - Pribadi",
        " - Work",
        " - Kerja",
        " - Default",
        " - Guest",
        " - Tamu",
    ];
    for psfx in &profile_suffixes {
        if let Some(stripped) = t.strip_suffix(psfx) {
            t = stripped.trim();
            break;
        }
    }

    // Also strip generic " - Profile N" / " - Profil N"
    if let Some((head, tail)) = t.rsplit_once(" - ") {
        let lower_tail = tail.trim().to_ascii_lowercase();
        if lower_tail.starts_with("profile ")
            || lower_tail.starts_with("profil ")
            || lower_tail == "personal"
            || lower_tail == "work"
            || lower_tail == "default"
        {
            t = head.trim();
        }
    }

    if t.is_empty() {
        return process_name.replace(".exe", "");
    }

    // YouTube handling
    let lower_t = t.to_ascii_lowercase();
    if lower_t == "youtube" {
        return "YouTube".to_string();
    }
    if let Some(video) = t.strip_suffix(" - YouTube") {
        let clean_video = video.trim();
        if !clean_video.is_empty() {
            return truncate_topic(&format!("YouTube: {}", clean_video), 30);
        }
    }
    if let Some(video) = t.strip_prefix("YouTube - ") {
        let clean_video = video.trim();
        if !clean_video.is_empty() {
            return truncate_topic(&format!("YouTube: {}", clean_video), 30);
        }
    }
    if lower_t.contains("youtube") && !lower_t.starts_with("youtube:") {
        return truncate_topic(&format!("YouTube: {}", t), 30);
    }

    // General web page title
    // Strip trailing site name if formatted like "Page Title | Site"
    if let Some((first, _)) = t.split_once(" | ") {
        let trimmed = first.trim();
        if !trimmed.is_empty() {
            t = trimmed;
        }
    }

    truncate_topic(t, 30)
}

fn truncate_topic(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let cut: String = text.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{cut}...")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_browser_title() {
        let res = clean_browser_title(
            "Never Gonna Give You Up - YouTube - Personal - Google Chrome",
            "chrome.exe",
        );
        assert_eq!(res, "YouTube: Never Gonna Give Y...");

        let res2 = clean_browser_title(
            "YouTube - Personal - Microsoft Edge",
            "msedge.exe",
        );
        assert_eq!(res2, "YouTube");

        let res3 = clean_browser_title(
            "Figma - Personal - Google Chrome",
            "chrome.exe",
        );
        assert_eq!(res3, "Figma");

        let res4 = clean_browser_title(
            "Pull Requests · repo/name - Work - Google Chrome",
            "chrome.exe",
        );
        assert_eq!(res4, "Pull Requests · repo/name");
    }
}
