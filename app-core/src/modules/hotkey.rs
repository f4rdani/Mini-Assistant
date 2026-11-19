use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY, WM_USER};

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_NOREPEAT: u32 = 0x4000;

pub const WM_RELOAD_HOTKEY: u32 = WM_USER + 101;
pub const WM_RELOAD_TRANS_HOTKEY: u32 = WM_USER + 102;
const HOTKEY_ID: i32 = 9001;
const TRANSLATE_HOTKEY_ID: i32 = 9002;

unsafe extern "system" {
    fn RegisterHotKey(
        hwnd: *mut core::ffi::c_void,
        id: i32,
        fsmodifiers: u32,
        vk: u32,
    ) -> i32;

    fn UnregisterHotKey(hwnd: *mut core::ffi::c_void, id: i32) -> i32;

    fn GetCurrentThreadId() -> u32;

    fn PostThreadMessageW(
        idthread: u32,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> i32;
}

type HotkeyCallback = Box<dyn Fn() + Send + 'static>;

static RUNNING: AtomicBool = AtomicBool::new(true);
static HOTKEY_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static CURRENT_MOD: AtomicU32 = AtomicU32::new(MOD_ALT | MOD_NOREPEAT);
static CURRENT_VK: AtomicU32 = AtomicU32::new(0x51); // 'Q'
static CURRENT_TRANS_MOD: AtomicU32 = AtomicU32::new(MOD_ALT | MOD_NOREPEAT);
static CURRENT_TRANS_VK: AtomicU32 = AtomicU32::new(0x54); // 'T'
static TRANSLATE_CALLBACK: OnceLock<Mutex<Option<HotkeyCallback>>> = OnceLock::new();

pub fn parse_modifier_code(modifier: &str) -> u32 {
    let mut mods = MOD_NOREPEAT;
    match modifier.to_lowercase().as_str() {
        "ctrl" | "control" => mods |= MOD_CONTROL,
        "shift" => mods |= MOD_SHIFT,
        "ctrl_shift" | "ctrl+shift" => mods |= MOD_CONTROL | MOD_SHIFT,
        "ctrl_alt" | "ctrl+alt" | "alt_ctrl" => mods |= MOD_CONTROL | MOD_ALT,
        "alt_shift" | "alt+shift" => mods |= MOD_ALT | MOD_SHIFT,
        _ => mods |= MOD_ALT, // default Alt
    }
    mods
}

pub fn parse_key_code(key: &str) -> u32 {
    match key.to_uppercase().as_str() {
        "SPACE" => 0x20,
        "TILDE" | "`" | "~" => 0xC0,
        "TAB" => 0x09,
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        "A" => 0x41,
        "B" => 0x42,
        "C" => 0x43,
        "D" => 0x44,
        "E" => 0x45,
        "F" => 0x46,
        "G" => 0x47,
        "H" => 0x48,
        "I" => 0x49,
        "J" => 0x4A,
        "K" => 0x4B,
        "L" => 0x4C,
        "M" => 0x4D,
        "N" => 0x4E,
        "O" => 0x4F,
        "P" => 0x50,
        "Q" => 0x51,
        "R" => 0x52,
        "S" => 0x53,
        "T" => 0x54,
        "U" => 0x55,
        "V" => 0x56,
        "W" => 0x57,
        "X" => 0x58,
        "Y" => 0x59,
        "Z" => 0x5A,
        _ => 0x51, // default Q
    }
}

pub fn update_global_hotkey(modifier: &str, key: &str) -> Result<(), String> {
    let mods = parse_modifier_code(modifier);
    let vk = parse_key_code(key);

    CURRENT_MOD.store(mods, Ordering::SeqCst);
    CURRENT_VK.store(vk, Ordering::SeqCst);

    let thread_id = HOTKEY_THREAD_ID.load(Ordering::SeqCst);
    if thread_id > 0 {
        unsafe {
            PostThreadMessageW(thread_id, WM_RELOAD_HOTKEY, 0, 0);
        }
    }
    Ok(())
}

pub fn update_translate_hotkey(modifier: &str, key: &str) -> Result<(), String> {
    let mods = parse_modifier_code(modifier);
    let vk = parse_key_code(key);

    CURRENT_TRANS_MOD.store(mods, Ordering::SeqCst);
    CURRENT_TRANS_VK.store(vk, Ordering::SeqCst);

    let thread_id = HOTKEY_THREAD_ID.load(Ordering::SeqCst);
    if thread_id > 0 {
        unsafe {
            PostThreadMessageW(thread_id, WM_RELOAD_TRANS_HOTKEY, 0, 0);
        }
    }
    Ok(())
}

pub fn set_translate_hotkey_callback(on_translate: Box<dyn Fn() + Send + 'static>) {
    let lock = TRANSLATE_CALLBACK.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        *guard = Some(on_translate);
    }
}

/// Runs a Win32 message loop on its own thread. `on_summon` is called every
/// time the hotkey fires (already after the foreground window was recorded).
pub fn start_global_hotkey_listener(on_summon: Box<dyn Fn() + Send + 'static>) {
    thread::spawn(move || unsafe {
        let tid = GetCurrentThreadId();
        HOTKEY_THREAD_ID.store(tid, Ordering::SeqCst);

        let mut current_m = CURRENT_MOD.load(Ordering::SeqCst);
        let mut current_v = CURRENT_VK.load(Ordering::SeqCst);
        let mut trans_m = CURRENT_TRANS_MOD.load(Ordering::SeqCst);
        let mut trans_v = CURRENT_TRANS_VK.load(Ordering::SeqCst);

        if RegisterHotKey(null_mut(), HOTKEY_ID, current_m, current_v) == 0 {
            #[cfg(debug_assertions)]
            eprintln!("[Global Hotkey] Gagal mendaftarkan hotkey awal");
        }
        if RegisterHotKey(null_mut(), TRANSLATE_HOTKEY_ID, trans_m, trans_v) == 0 {
            #[cfg(debug_assertions)]
            eprintln!("[Global Hotkey] Gagal mendaftarkan hotkey translate awal");
        }

        let mut msg: MSG = std::mem::zeroed();
        while RUNNING.load(Ordering::SeqCst) {
            if GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                if msg.message == WM_HOTKEY && msg.wParam == HOTKEY_ID as usize {
                    // 0. Rekam window aktif pengguna (misal game) sebelum Mini Assistant mengambil fokus
                    crate::modules::window_detector::record_foreground_window();

                    // The UI layer decides what to do (show window, switch mode).
                    on_summon();
                } else if msg.message == WM_HOTKEY && msg.wParam == TRANSLATE_HOTKEY_ID as usize {
                    crate::modules::window_detector::record_foreground_window();
                    if let Some(lock) = TRANSLATE_CALLBACK.get() {
                        if let Ok(guard) = lock.lock() {
                            if let Some(cb) = guard.as_ref() {
                                cb();
                            }
                        }
                    }
                } else if msg.message == WM_RELOAD_HOTKEY {
                    // Dinamis: Lepas hotkey lama dan daftarkan hotkey baru
                    UnregisterHotKey(null_mut(), HOTKEY_ID);
                    current_m = CURRENT_MOD.load(Ordering::SeqCst);
                    current_v = CURRENT_VK.load(Ordering::SeqCst);
                    let _ = RegisterHotKey(null_mut(), HOTKEY_ID, current_m, current_v);
                } else if msg.message == WM_RELOAD_TRANS_HOTKEY {
                    UnregisterHotKey(null_mut(), TRANSLATE_HOTKEY_ID);
                    trans_m = CURRENT_TRANS_MOD.load(Ordering::SeqCst);
                    trans_v = CURRENT_TRANS_VK.load(Ordering::SeqCst);
                    let _ = RegisterHotKey(null_mut(), TRANSLATE_HOTKEY_ID, trans_m, trans_v);
                }
            }
        }

        UnregisterHotKey(null_mut(), HOTKEY_ID);
        UnregisterHotKey(null_mut(), TRANSLATE_HOTKEY_ID);
    });
}

/// Posts the same message the OS sends when the hotkey is pressed. Used by the
/// test driver so the whole listener path (message loop -> callback) is
/// exercised without a physical keypress.
pub fn simulate_hotkey() -> bool {
    let tid = HOTKEY_THREAD_ID.load(Ordering::SeqCst);
    if tid == 0 {
        return false;
    }
    unsafe { PostThreadMessageW(tid, WM_HOTKEY, HOTKEY_ID as usize, 0) != 0 }
}

pub fn simulate_translate_hotkey() -> bool {
    let tid = HOTKEY_THREAD_ID.load(Ordering::SeqCst);
    if tid == 0 {
        return false;
    }
    unsafe { PostThreadMessageW(tid, WM_HOTKEY, TRANSLATE_HOTKEY_ID as usize, 0) != 0 }
}

/// Signals the background Win32 hotkey thread to unregister global hotkeys and exit.
pub fn stop_global_hotkeys() {
    RUNNING.store(false, Ordering::SeqCst);
    let tid = HOTKEY_THREAD_ID.swap(0, Ordering::SeqCst);
    if tid != 0 {
        const WM_QUIT: u32 = 0x0012;
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }
}
