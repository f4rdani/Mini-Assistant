use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::prelude::*;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use crate::modules::leveldb_reader;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Security::Cryptography::CryptUnprotectData;

#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn LocalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserProfileInfo {
    pub browser_type: String, // "chrome", "edge"
    pub browser_name: String, // "Google Chrome", "Microsoft Edge"
    pub profile_id: String,   // "Default", "Profile 1", etc.
    pub profile_name: String, // Friendly name or profile folder
    pub cookies_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieDiagnosticItem {
    pub name: String,
    pub host: String,
    pub encryption_type: String, // "Plaintext", "DPAPI (v10)", "App-Bound (v20)", "Legacy DPAPI"
    pub is_decrypted: bool,
    pub is_auth_token: bool,
    pub preview: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CookieSyncResult {
    pub success: bool,
    pub provider: String,
    pub browser_name: String,
    pub profile_name: String,
    pub total_found: usize,
    pub decrypted_count: usize,
    pub v20_count: usize,
    pub has_auth_token: bool,
    pub status_code: String, // "READY", "LOCKED", "NO_COOKIES", "APP_BOUND", "NOT_FOUND", "ERROR"
    pub message: String,
    pub cookie_items: Vec<CookieDiagnosticItem>,
    pub cookie_header: String,
    pub statsig_seed: Option<String>,
    pub statsig_hex: Option<String>,
    #[serde(default)]
    pub account_label: Option<String>,
    #[serde(skip_serializing)]
    pub cookies_map: HashMap<String, String>,
}

/// Scans local Windows filesystem for installed Google Chrome and Microsoft Edge profiles.
pub fn scan_available_browsers() -> Vec<BrowserProfileInfo> {
    let mut profiles = Vec::new();
    let local_app_data = match dirs::data_local_dir() {
        Some(p) => p,
        None => return profiles,
    };

    // 1. Google Chrome
    let chrome_dir = local_app_data.join("Google\\Chrome\\User Data");
    if chrome_dir.exists() {
        find_chromium_profiles(&chrome_dir, "chrome", "Google Chrome", &mut profiles);
    }

    // 2. Microsoft Edge
    let edge_dir = local_app_data.join("Microsoft\\Edge\\User Data");
    if edge_dir.exists() {
        find_chromium_profiles(&edge_dir, "edge", "Microsoft Edge", &mut profiles);
    }

    profiles
}

fn find_chromium_profiles(
    user_data_dir: &Path,
    browser_type: &str,
    browser_name: &str,
    out: &mut Vec<BrowserProfileInfo>,
) {
    if let Ok(entries) = fs::read_dir(user_data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();

            // Chromium profiles are "Default", "Profile 1", "Profile 2", etc.
            if dir_name == "Default" || dir_name.starts_with("Profile ") {
                let cookies_file = path.join("Network").join("Cookies");
                let alt_cookies = path.join("Cookies");
                let leveldb_dir = path.join("Local Storage").join("leveldb");

                let final_cookie_path = if cookies_file.exists() {
                    Some(cookies_file)
                } else if alt_cookies.exists() {
                    Some(alt_cookies)
                } else if leveldb_dir.exists() {
                    Some(cookies_file)
                } else {
                    None
                };

                if let Some(cookie_path) = final_cookie_path {
                    let pref_file = path.join("Preferences");
                    let custom_name = if pref_file.exists() {
                        if let Ok(content) = fs::read_to_string(&pref_file) {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                                json.get("profile")
                                    .and_then(|p| p.get("name"))
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let profile_name = match custom_name {
                        Some(name) if !name.is_empty() => format!("{} ({})", browser_name, name),
                        _ => format!("{} ({})", browser_name, dir_name),
                    };

                    out.push(BrowserProfileInfo {
                        browser_type: browser_type.to_string(),
                        browser_name: browser_name.to_string(),
                        profile_id: dir_name.clone(),
                        profile_name,
                        cookies_path: cookie_path.to_string_lossy().to_string(),
                    });
                }
            }
        }
    }
}

/// Decrypts DPAPI master key from Chromium Local State JSON
fn get_chromium_master_key(local_state_path: &Path) -> Result<Vec<u8>, String> {
    let content = fs::read_to_string(local_state_path)
        .map_err(|e| format!("Failed to read Local State: {}", e))?;
    let json: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse Local State JSON: {}", e))?;

    let encrypted_key_b64 = json["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| "encrypted_key not found in Local State".to_string())?;

    let encrypted_bytes = BASE64_STANDARD
        .decode(encrypted_key_b64)
        .map_err(|e| format!("Failed to base64 decode master key: {}", e))?;

    // First 5 bytes are "DPAPI"
    if encrypted_bytes.len() < 5 || &encrypted_bytes[..5] != b"DPAPI" {
        return Err("Invalid DPAPI header in encrypted_key".to_string());
    }

    let raw_payload = &encrypted_bytes[5..];
    decrypt_dpapi_blob(raw_payload)
}

/// Decrypts DPAPI data blob on Windows
fn decrypt_dpapi_blob(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let mut in_blob = DataBlob {
            cb_data: data.len() as u32,
            pb_data: data.as_ptr() as *mut u8,
        };
        let mut out_blob = DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };

        let success = CryptUnprotectData(
            &mut in_blob as *mut _ as *mut _,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut out_blob as *mut _ as *mut _,
        );

        if success == 0 || out_blob.pb_data.is_null() {
            return Err("CryptUnprotectData failed to decrypt master key".to_string());
        }

        let slice = std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize);
        let result = slice.to_vec();

        LocalFree(out_blob.pb_data as _);
        Ok(result)
    }
}

/// Decrypts AES-256-GCM cookie values in Chromium
fn decrypt_chromium_cookie_value(encrypted: &[u8], master_key: &[u8]) -> Result<String, String> {
    if encrypted.len() < 15 {
        return Ok(String::new());
    }

    let prefix = &encrypted[..3];
    if prefix == b"v10" || prefix == b"v11" {
        let nonce_bytes = &encrypted[3..15];
        let ciphertext = &encrypted[15..];

        let cipher = Aes256Gcm::new_from_slice(master_key)
            .map_err(|e| format!("Invalid AES key: {}", e))?;
        let nonce = Nonce::from_slice(nonce_bytes);

        let decrypted = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| format!("AES decrypt failed: {}", e))?;

        String::from_utf8(decrypted).map_err(|e| format!("UTF8 error: {}", e))
    } else {
        // Fallback for older non-v10 DPAPI encrypted cookies
        let dec = decrypt_dpapi_blob(encrypted)?;
        String::from_utf8(dec).map_err(|e| format!("UTF8 error: {}", e))
    }
}

/// Membaca berkas dengan flag FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE pada Windows
/// agar dapat membaca berkas tanpa gagal (os error 32) saat berkas dibuka oleh browser yang berjalan.
pub fn read_file_shared(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(windows)]
    {
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7) // FILE_SHARE_READ (1) | FILE_SHARE_WRITE (2) | FILE_SHARE_DELETE (4)
            .open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }
    #[cfg(not(windows))]
    {
        fs::read(path)
    }
}

/// Menyalin berkas dengan aman secara NON-DESTRUKTIF tanpa pernah mematikan proses browser.
/// Coba fs::copy biasa, lalu fallback read_file_shared dengan retry singkat untuk lock transient SQLite.
pub fn copy_file_shared(src: &Path, dst: &Path) -> Result<(), String> {
    if fs::copy(src, dst).is_ok() {
        return Ok(());
    }

    // Coba baca berkas secara non-eksklusif dengan retry jika browser sedang aktif menulis
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(75));
        }
        if let Ok(bytes) = read_file_shared(src) {
            return fs::write(dst, bytes).map_err(|e| e.to_string());
        }
    }

    Err("Berkas database cookies sedang dikunci secara eksklusif oleh browser.".to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenVerifyStatus {
    Valid(String),         // e.g. "Fardani Azhar" atau email akun
    Invalid(String),       // e.g. "DeepSeek menolak token (40003)"
    NetworkError(String),  // e.g. offline atau timeout
}

/// Memvalidasi token ke endpoint resmi DeepSeek (`https://chat.deepseek.com/api/v0/users/current`).
pub fn verify_deepseek_token(token: &str) -> TokenVerifyStatus {
    let trimmed = token.trim();
    if trimmed.len() < 20 {
        return TokenVerifyStatus::Invalid("Token terlalu pendek".to_string());
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return TokenVerifyStatus::NetworkError(e.to_string()),
    };

    let mut resp_opt = None;
    let mut last_err = String::new();
    for attempt in 0..2 {
        match client
            .get("https://chat.deepseek.com/api/v0/users/current")
            .header("authorization", format!("Bearer {}", trimmed))
            .header("origin", "https://chat.deepseek.com")
            .header("referer", "https://chat.deepseek.com/")
            .header("x-client-platform", "web")
            .header("accept", "*/*")
            .header(
                "user-agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
            )
            .send()
        {
            Ok(r) => {
                resp_opt = Some(r);
                break;
            }
            Err(e) => {
                last_err = format!("Gagal menghubungi server DeepSeek: {e}");
                if attempt == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }
    let resp = match resp_opt {
        Some(r) => r,
        None => return TokenVerifyStatus::NetworkError(last_err),
    };

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let val: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return TokenVerifyStatus::NetworkError(format!("Respon DeepSeek bukan JSON (HTTP {status})")),
    };

    let code = val.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code == 0 {
        let name = val
            .pointer("/data/biz_data/email")
            .filter(|s| s.as_str().map(|str| !str.trim().is_empty()).unwrap_or(false))
            .or_else(|| val.pointer("/data/biz_data/id_profile/name"))
            .and_then(|s| s.as_str())
            .unwrap_or("Akun Terautentikasi");
        TokenVerifyStatus::Valid(name.to_string())
    } else {
        let msg = val.get("msg").and_then(|m| m.as_str()).unwrap_or("Token tidak valid");
        TokenVerifyStatus::Invalid(format!("DeepSeek menolak token ({code}): {msg}"))
    }
}

/// Detailed diagnostic and extraction routine for a single AI provider
pub fn diagnose_cookie_extraction(
    profile: &BrowserProfileInfo,
    provider: &str,
) -> CookieSyncResult {
    let cookie_db_path = PathBuf::from(&profile.cookies_path);

    // Check Local Storage LevelDB first (unencrypted in Chromium 127+, bypasses v20 App-Bound)
    let profile_dir: Option<PathBuf> = if cookie_db_path.parent().map(|p| p.file_name().unwrap_or_default() == "Network").unwrap_or(false) {
        cookie_db_path.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf())
    } else {
        cookie_db_path.parent().map(|p| p.to_path_buf())
    };

    let mut deepseek_user_name: Option<String> = None;
    let mut deepseek_expired_reason: Option<String> = None;

    let leveldb_token = if provider == "deepseek" {
        if let Some(ref pdir) = profile_dir {
            let candidates = extract_deepseek_candidates(pdir);
            let mut found = None;
            for cand in &candidates {
                match verify_deepseek_token(cand) {
                    TokenVerifyStatus::Valid(name) => {
                        deepseek_user_name = Some(name);
                        found = Some(cand.clone());
                        break;
                    }
                    TokenVerifyStatus::NetworkError(_) => {
                        // Jika offline, gunakan kandidat terbaru sebagai best-effort fallback
                        deepseek_user_name = Some("Offline".to_string());
                        found = Some(cand.clone());
                        break;
                    }
                    TokenVerifyStatus::Invalid(err) => {
                        deepseek_expired_reason = Some(err);
                    }
                }
            }
            found
        } else {
            None
        }
    } else {
        profile_dir.as_deref().and_then(|pdir| extract_from_leveldb(pdir, provider))
    };

    let mut result = if !cookie_db_path.exists() {
        CookieSyncResult {
            success: false,
            provider: provider.to_string(),
            browser_name: profile.browser_name.clone(),
            profile_name: profile.profile_name.clone(),
            status_code: "NOT_FOUND".to_string(),
            message: format!("Berkas database cookies tidak ditemukan: {}", profile.cookies_path),
            ..Default::default()
        }
    } else {
        // Temporary copy to bypass file lock if browser is open
        let temp_dir = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let temp_db = temp_dir.join(format!("sq_cookies_{}_{}.tmp", std::process::id(), nonce));

        match copy_file_shared(&cookie_db_path, &temp_db) {
            Err(e) => {
                let err_str = e.to_string();
                let is_locked = err_str.contains("os error 32")
                    || err_str.contains("used by another process")
                    || err_str.contains("sharing violation");
                let (status_code, message) = if is_locked {
                    (
                        "LOCKED".to_string(),
                        format!(
                            "Berkas cookies sedang dikunci oleh '{}'.",
                            profile.browser_name
                        ),
                    )
                } else {
                    (
                        "ERROR".to_string(),
                        format!("Gagal menyalin berkas cookies: {}", err_str),
                    )
                };

                CookieSyncResult {
                    success: false,
                    provider: provider.to_string(),
                    browser_name: profile.browser_name.clone(),
                    profile_name: profile.profile_name.clone(),
                    status_code,
                    message,
                    ..Default::default()
                }
            }
            Ok(_) => {
                // Also attempt copying WAL file if present
                let wal_src = cookie_db_path.with_file_name(format!(
                    "{}-wal",
                    cookie_db_path.file_name().unwrap_or_default().to_string_lossy()
                ));
                let wal_dst = temp_dir.join(format!(
                    "{}-wal",
                    temp_db.file_name().unwrap_or_default().to_string_lossy()
                ));
                if wal_src.exists() {
                    let _ = copy_file_shared(&wal_src, &wal_dst);
                }

                // Chromium based (Chrome, Edge)
                let user_data_dir = cookie_db_path
                    .ancestors()
                    .find(|p| p.join("Local State").exists());

                let master_key = match user_data_dir {
                    Some(dir) => get_chromium_master_key(&dir.join("Local State")).ok(),
                    None => None,
                };

                let res = extract_chromium_cookies(&temp_db, profile, master_key.as_deref(), provider);

                // Clean up temporary files
                let _ = fs::remove_file(&temp_db);
                let _ = fs::remove_file(&wal_dst);

                res
            }
        }
    };

    // If LevelDB found an active auth token, upgrade the result!
    if let Some(token) = leveldb_token {
        result.success = true;
        result.has_auth_token = true;
        result.status_code = "READY".to_string();
        result.account_label = deepseek_user_name.clone();
        let display_suffix = if let Some(ref name) = deepseek_user_name {
            format!(" ({})", name)
        } else {
            String::new()
        };
        result.message = format!(
            "Berhasil mengekstrak token sesi aktif {}{} dari LocalStorage {}!",
            provider.to_uppercase(),
            display_suffix,
            profile.browser_name
        );
        if result.cookie_header.is_empty() {
            result.cookie_header = token.clone();
        }
        let token_key = match provider {
            "deepseek" => "userToken",
            "chatgpt" => "accessToken",
            _ => "auth_token",
        };
        result.cookies_map.insert(token_key.to_string(), token.clone());
        result.cookie_items.insert(
            0,
            CookieDiagnosticItem {
                name: token_key.to_string(),
                host: format!("{}.com", provider),
                encryption_type: "LocalStorage (Terverifikasi)".to_string(),
                is_decrypted: true,
                is_auth_token: true,
                preview: format!("{} (LocalStorage aktif{})", token_key, display_suffix),
            },
        );
    } else if provider == "deepseek" {
        result.success = false;
        result.has_auth_token = false;
        if let Some(err_msg) = deepseek_expired_reason {
            result.status_code = "INVALID_TOKEN".to_string();
            result.message = format!(
                "Token DeepSeek ditemukan di {} (LocalStorage), namun {}. Silakan tekan Ctrl+F5 pada tab https://chat.deepseek.com di {} atau pastikan Anda sudah login.",
                profile.browser_name, err_msg, profile.browser_name
            );
        } else {
            result.status_code = "NO_COOKIES".to_string();
            result.message = format!(
                "Tidak ditemukan sesi login DeepSeek aktif di {}. Pastikan Anda telah membuka https://chat.deepseek.com dan login ke akun Anda di {}.",
                profile.browser_name, profile.browser_name
            );
        }
    }

    result
}

/// Turunkan indeks ke batas karakter terdekat agar slice &str tidak panic
/// pada data biner LevelDB.
fn floor_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod deepseek_leveldb_tests {
    #[test]
    fn extracts_latest_deepseek_token_from_snappy_ldb_and_wal_ignoring_other_origins() {
        fn enc_varint(mut v: u64, dst: &mut Vec<u8>) {
            while v >= 0x80 {
                dst.push((v as u8 & 0x7f) | 0x80);
                v >>= 7;
            }
            dst.push(v as u8);
        }
        fn make_sstable(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
            let mut blk = Vec::new();
            let mut prev: &[u8] = &[];
            for (k, v) in entries {
                let sh = prev.iter().zip(k.iter()).take_while(|(a, b)| a == b).count();
                enc_varint(sh as u64, &mut blk);
                enc_varint((k.len() - sh) as u64, &mut blk);
                enc_varint(v.len() as u64, &mut blk);
                blk.extend_from_slice(&k[sh..]);
                blk.extend_from_slice(v);
                prev = k;
            }
            blk.extend_from_slice(&0u32.to_le_bytes());
            blk.extend_from_slice(&1u32.to_le_bytes());

            let mut snap = Vec::new();
            enc_varint(blk.len() as u64, &mut snap);
            if blk.len() <= 60 {
                snap.push((blk.len() as u8 - 1) << 2);
            } else {
                snap.push(61 << 2);
                snap.extend_from_slice(&((blk.len() - 1) as u16).to_le_bytes());
            }
            snap.extend_from_slice(&blk);

            let mut out = Vec::new();
            let d_sz = snap.len() as u64;
            out.extend_from_slice(&snap);
            out.push(1);
            out.extend_from_slice(&[0u8; 4]);

            let mut h = Vec::new();
            enc_varint(0, &mut h);
            enc_varint(d_sz, &mut h);
            let mut idx = Vec::new();
            let lk = entries.last().map(|(k, _)| *k).unwrap_or(&[]);
            enc_varint(0, &mut idx);
            enc_varint(lk.len() as u64, &mut idx);
            enc_varint(h.len() as u64, &mut idx);
            idx.extend_from_slice(lk);
            idx.extend_from_slice(&h);
            idx.extend_from_slice(&0u32.to_le_bytes());
            idx.extend_from_slice(&1u32.to_le_bytes());
            let i_off = out.len() as u64;
            let i_sz = idx.len() as u64;
            out.extend_from_slice(&idx);
            out.push(0);
            out.extend_from_slice(&[0u8; 4]);

            let mut f = Vec::new();
            enc_varint(0, &mut f);
            enc_varint(0, &mut f);
            enc_varint(i_off, &mut f);
            enc_varint(i_sz, &mut f);
            while f.len() < 40 {
                f.push(0);
            }
            f.extend_from_slice(&[0x57, 0xfb, 0x80, 0x8b, 0x24, 0x75, 0x47, 0xdb]);
            out.extend_from_slice(&f);
            out
        }

        let tmp = std::env::temp_dir().join(format!("ds_ldb_test_{}", std::process::id()));
        let ldb_dir = tmp.join("Local Storage").join("leveldb");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&ldb_dir).unwrap();

        let mut k_chat_new = b"_https://chat.deepseek.com\x00\x01userToken".to_vec();
        k_chat_new.extend_from_slice(&((500u64 << 8) | 1u64).to_le_bytes());
        let mut k_chat_null = b"_https://chat.deepseek.com\x00\x01userToken".to_vec();
        k_chat_null.extend_from_slice(&((400u64 << 8) | 1u64).to_le_bytes());
        let mut k_chat_old = b"_https://chat.deepseek.com\x00\x01userToken".to_vec();
        k_chat_old.extend_from_slice(&((300u64 << 8) | 1u64).to_le_bytes());
        let mut k_plat = b"_https://platform.deepseek.com\x00\x01userToken".to_vec();
        k_plat.extend_from_slice(&((999u64 << 8) | 1u64).to_le_bytes());

        let v_chat_new = b"\x01{\"value\":\"NEW_ACTIVE_DEEPSEEK_CHAT_TOKEN_1234567890\",\"__version\":\"0\"}";
        let v_chat_null = b"\x01{\"value\":null,\"__version\":\"0\"}";
        let v_chat_old = b"\x01{\"value\":\"OLD_EXPIRED_DEEPSEEK_CHAT_TOKEN_0987654321\",\"__version\":\"0\"}";
        let v_plat = b"\x01{\"value\":\"PLATFORM_TOKEN_SHOULD_NOT_BE_PICKED_12345\",\"__version\":\"0\"}";

        let sstable = make_sstable(&[
            (&k_chat_new, v_chat_new),
            (&k_chat_null, v_chat_null),
            (&k_chat_old, v_chat_old),
            (&k_plat, v_plat),
        ]);
        std::fs::write(ldb_dir.join("000988.ldb"), &sstable).unwrap();

        let cands = super::extract_deepseek_candidates(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(!cands.is_empty());
        assert_eq!(cands[0], "NEW_ACTIVE_DEEPSEEK_CHAT_TOKEN_1234567890");
        assert!(cands.iter().any(|c| c == "OLD_EXPIRED_DEEPSEEK_CHAT_TOKEN_0987654321"));
        assert!(!cands.iter().any(|c| c == "PLATFORM_TOKEN_SHOULD_NOT_BE_PICKED_12345"));
    }

}

fn read_leveldb_bytes_shared(file_path: &Path) -> Option<Vec<u8>> {
    match read_file_shared(file_path) {
        Ok(d) if !d.is_empty() => Some(d),
        Ok(_) => None,
        Err(_) => {
            let temp_copy = std::env::temp_dir().join(format!(
                "sq_ldb_{}_{}.tmp",
                std::process::id(),
                file_path.file_name().unwrap_or_default().to_string_lossy()
            ));
            if copy_file_shared(file_path, &temp_copy).is_ok() {
                let d = read_file_shared(&temp_copy).ok();
                let _ = fs::remove_file(temp_copy);
                d.filter(|v| !v.is_empty())
            } else {
                None
            }
        }
    }
}

fn leveldb_search_blobs(file: &leveldb_reader::LevelDbFile) -> Vec<Vec<u8>> {
    if file.is_ldb {
        if let Some((recs, decompressed)) = leveldb_reader::parse_sstable(&file.data) {
            let mut flat = Vec::with_capacity(decompressed.len());
            for rec in recs {
                if rec.is_put {
                    flat.extend_from_slice(&rec.user_key);
                    flat.push(b' ');
                    flat.extend_from_slice(&rec.value);
                    flat.push(b'\n');
                }
            }
            if !flat.is_empty() {
                return vec![flat, file.data.clone()];
            }
        }
    }
    vec![file.data.clone()]
}

fn is_deepseek_chat_origin(origin: &str) -> bool {
    let trimmed = origin.trim_end_matches('/');
    trimmed.eq_ignore_ascii_case("https://chat.deepseek.com")
        || trimmed.eq_ignore_ascii_case("http://chat.deepseek.com")
}

fn is_valid_deepseek_token_shape(tok: &str) -> bool {
    let len = tok.len();
    (20..=1024).contains(&len)
        && !tok.contains("userToken")
        && !tok.contains("http")
        && !tok.contains("__version")
        && !tok.contains("value")
        && tok.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '+' | '=' | '-' | '_' | '.')
        })
}

fn parse_deepseek_user_token_value(raw_bytes: &[u8]) -> Option<String> {
    let val_str = leveldb_reader::decode_chrome_ls_string(raw_bytes)?;
    let trimmed = val_str.trim().trim_matches('\0').trim();
    if trimmed.is_empty() {
        return None;
    }
    let extracted = if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(inner) = v.get("value") {
            inner.as_str().map(str::to_string)?
        } else if let Some(s) = v.as_str() {
            s.to_string()
        } else {
            return None;
        }
    } else {
        trimmed.to_string()
    };
    let cleaned = extracted.trim().to_string();
    if is_valid_deepseek_token_shape(&cleaned) {
        Some(cleaned)
    } else {
        None
    }
}

fn collect_heuristic_deepseek_candidates(blob: &[u8], out: &mut Vec<String>, limit: usize) {
    let text = String::from_utf8_lossy(blob);

    // Pola 1: DeepSeek Storage Wrapper `value":"<token>"`
    let mut search_from = 0;
    while let Some(rel_pos) = text[search_from..].find("value\":\"") {
        let pos = search_from + rel_pos + 8;
        if let Some(quote_end) = text[pos..].find('"') {
            let candidate = &text[pos..pos + quote_end];
            if candidate.len() >= 48 && is_valid_deepseek_token_shape(candidate) {
                let ctx_start = floor_boundary(&text, pos.saturating_sub(240));
                let ctx = &text[ctx_start..pos];
                if !ctx.contains("platform.deepseek.com") && !ctx.contains("esimplus.me") {
                    let cand = candidate.to_string();
                    if !out.contains(&cand) {
                        out.push(cand);
                        if out.len() >= limit {
                            return;
                        }
                    }
                }
            }
        }
        search_from = pos + 1;
    }

    // Pola 2: String "userToken" eksplisit
    let mut search_from = 0;
    while let Some(rel_pos) = text[search_from..].find("userToken") {
        let pos = search_from + rel_pos;
        let ctx_start = floor_boundary(&text, pos.saturating_sub(100));
        let ctx = &text[ctx_start..pos];
        if ctx.contains("platform.deepseek.com") || ctx.contains("esimplus.me") {
            search_from = pos + 9;
            continue;
        }
        let end_pos = floor_boundary(&text, (pos + 400).min(text.len()));
        let slice = &text[pos..end_pos];
        for part in slice.split('"') {
            let trimmed = part.trim();
            if trimmed.len() >= 40 && is_valid_deepseek_token_shape(trimmed) {
                let cand = trimmed.to_string();
                if !out.contains(&cand) {
                    out.push(cand);
                    if out.len() >= limit {
                        return;
                    }
                }
            }
        }
        search_from = pos + 9;
    }
}

/// Kumpulkan semua kandidat token DeepSeek dari LevelDB (`.ldb` Snappy + `.log` WAL,
/// diurutkan berdasarkan sequence number / berkas terbaru ke terlama).
pub fn extract_deepseek_candidates(profile_dir: &Path) -> Vec<String> {
    let files = leveldb_reader::read_leveldb_files(profile_dir, read_leveldb_bytes_shared);
    if files.is_empty() {
        return Vec::new();
    }

    // Tuple: (seq, file_mtime, token) so active newest sequence wins first.
    let mut structured: Vec<(u64, Option<std::time::SystemTime>, String)> = Vec::new();
    let mut fallback_blobs: Vec<Vec<u8>> = Vec::new();

    for file in &files {
        let records = if file.is_ldb {
            if let Some((recs, _)) = leveldb_reader::parse_sstable(&file.data) {
                recs
            } else {
                fallback_blobs.push(file.data.clone());
                Vec::new()
            }
        } else {
            let wal = leveldb_reader::parse_wal_records(&file.data);
            if wal.is_empty() {
                fallback_blobs.push(file.data.clone());
            }
            wal
        };

        for rec in records {
            if !rec.is_put {
                continue;
            }
            let Some((origin, key_name)) = leveldb_reader::parse_chrome_ls_key(&rec.user_key) else {
                continue;
            };
            if is_deepseek_chat_origin(&origin) && key_name == "userToken" {
                if let Some(tok) = parse_deepseek_user_token_value(&rec.value) {
                    structured.push((rec.seq, file.modified, tok));
                }
            }
        }
    }

    structured.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut out: Vec<String> = Vec::new();
    for (_, _, tok) in structured {
        if !out.contains(&tok) {
            out.push(tok);
            if out.len() >= 10 {
                return out;
            }
        }
    }

    for blob in &fallback_blobs {
        if out.len() >= 10 {
            break;
        }
        collect_heuristic_deepseek_candidates(blob, &mut out, 10);
    }

    out
}

pub fn extract_from_leveldb(profile_dir: &Path, provider: &str) -> Option<String> {
    if provider == "deepseek" {
        let candidates = extract_deepseek_candidates(profile_dir);
        for cand in candidates {
            match verify_deepseek_token(&cand) {
                TokenVerifyStatus::Valid(_) => return Some(cand),
                TokenVerifyStatus::NetworkError(_) => return Some(cand),
                TokenVerifyStatus::Invalid(_) => continue,
            }
        }
        return None;
    }

    for file in leveldb_reader::read_leveldb_files(profile_dir, read_leveldb_bytes_shared) {
        for data in leveldb_search_blobs(&file) {
            if provider == "chatgpt" {
                    // Only extract tokens belonging to chatgpt or openai
                    if !data.windows(b"chatgpt".len()).any(|w| w.eq_ignore_ascii_case(b"chatgpt"))
                        && !data.windows(b"openai".len()).any(|w| w.eq_ignore_ascii_case(b"openai")) {
                        continue;
                    }
                    let needle = b"eyJhbGciOi";
                    let mut search_from = 0;
                    while let Some(rel_pos) = data[search_from..].windows(needle.len()).position(|w| w == needle) {
                        let pos = search_from + rel_pos;
                        let s_min = pos.saturating_sub(120);
                        let context_str = String::from_utf8_lossy(&data[s_min..pos]).to_lowercase();
                        if context_str.contains("chatgpt") || context_str.contains("openai") || context_str.contains("accesstoken") || context_str.contains("token") {
                            let end_pos = (pos + 1200).min(data.len());
                            let slice = &data[pos..end_pos];
                            let jwt: String = slice
                                .iter()
                                .take_while(|&&c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_')
                                .map(|&c| c as char)
                                .collect();
                            if jwt.len() > 50 {
                                return Some(jwt);
                            }
                        }
                        search_from = pos + 10;
                    }
            }
        }
    }

    None
}

fn extract_chromium_cookies(
    db_path: &Path,
    profile: &BrowserProfileInfo,
    master_key: Option<&[u8]>,
    provider: &str,
) -> CookieSyncResult {
    let conn = match Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ).or_else(|_| Connection::open(db_path)) {
        Ok(c) => c,
        Err(e) => {
            return CookieSyncResult {
                success: false,
                provider: provider.to_string(),
                browser_name: profile.browser_name.clone(),
                profile_name: profile.profile_name.clone(),
                total_found: 0,
                decrypted_count: 0,
                v20_count: 0,
                has_auth_token: false,
                status_code: "ERROR".to_string(),
                message: format!("Gagal membuka SQLite cookies: {}", e),
                cookie_items: Vec::new(),
                cookie_header: String::new(),
                statsig_seed: None,
                statsig_hex: None,
                account_label: None,
                cookies_map: HashMap::new(),
            };
        }
    };

    let host_condition = match provider {
        "deepseek" => "(host_key LIKE '%deepseek.com%')",
        "grok" => "(host_key LIKE '%.x.com%' OR host_key LIKE '%grok.com%' OR host_key LIKE '%.twitter.com%')",
        "claude" => "(host_key LIKE '%claude.ai%' OR host_key LIKE '%anthropic.com%')",
        "gemini" => "(host_key LIKE '%.google.com%')",
        "chatgpt" => "(host_key LIKE '%chatgpt.com%' OR host_key LIKE '%.openai.com%')",
        _ => "1=1",
    };

    let query = format!(
        "SELECT host_key, name, value, encrypted_value FROM cookies WHERE {}",
        host_condition
    );

    let mut stmt = match conn.prepare(&query) {
        Ok(s) => s,
        Err(e) => {
            return CookieSyncResult {
                success: false,
                provider: provider.to_string(),
                browser_name: profile.browser_name.clone(),
                profile_name: profile.profile_name.clone(),
                total_found: 0,
                decrypted_count: 0,
                v20_count: 0,
                has_auth_token: false,
                status_code: "SYNTAX_ERROR".to_string(),
                message: format!("Query SQLite gagal: {}", e),
                cookie_items: Vec::new(),
                cookie_header: String::new(),
                statsig_seed: None,
                statsig_hex: None,
                account_label: None,
                cookies_map: HashMap::new(),
            };
        }
    };

    let mut cookies_map: HashMap<String, String> = HashMap::new();
    let mut diagnostic_items: Vec<CookieDiagnosticItem> = Vec::new();
    let mut decrypted_count = 0;
    let mut v20_count = 0;
    let mut total_found = 0;
    let mut has_auth_token = false;

    let rows = match stmt.query_map([], |row| {
        let host: String = row.get(0)?;
        let name: String = row.get(1)?;
        let plain_val: String = row.get(2).unwrap_or_default();
        let enc_val: Vec<u8> = row.get(3).unwrap_or_default();
        Ok((host, name, plain_val, enc_val))
    }) {
        Ok(r) => r,
        Err(e) => {
            return CookieSyncResult {
                success: false,
                provider: provider.to_string(),
                browser_name: profile.browser_name.clone(),
                profile_name: profile.profile_name.clone(),
                total_found: 0,
                decrypted_count: 0,
                v20_count: 0,
                has_auth_token: false,
                status_code: "ERROR".to_string(),
                message: format!("Gagal membaca baris tabel cookies: {}", e),
                cookie_items: Vec::new(),
                cookie_header: String::new(),
                statsig_seed: None,
                statsig_hex: None,
                account_label: None,
                cookies_map: HashMap::new(),
            };
        }
    };

    for row_res in rows.flatten() {
        let (host, name, plain_val, enc_val) = row_res;
        total_found += 1;

        let is_auth = match provider {
            "deepseek" => name == "userToken" || name == "d_token" || name == "token" || name.contains("session") || name == "HWWAFSESID",
            "grok" => name == "auth_token" || name == "ct0" || name == "twid"
                || name == "sso" || name == "sso-rw"
                || name.contains("statsig") || name == "statsig_seed" || name == "statsig_hex"
                || name.contains("grok"),
            "chatgpt" => name.contains("session-token") || name == "oai-did" || name == "_puid" || name == "cf_clearance",
            "claude" => name.contains("session") || name == "sessionKey" || name == "cf_clearance",
            "gemini" => name.contains("SID") || name.contains("PSID") || name == "__Secure-BUCKET",
            _ => false,
        };

        if is_auth {
            has_auth_token = true;
        }

        let mut is_decrypted = false;
        let mut enc_type = "Unknown".to_string();
        let mut preview = String::new();

        if !plain_val.is_empty() {
            enc_type = "Plaintext".to_string();
            is_decrypted = true;
            decrypted_count += 1;
            preview = format!("{} (tersedia)", name);
            cookies_map.insert(name.clone(), plain_val);
        } else if !enc_val.is_empty() {
            if enc_val.starts_with(b"v20") {
                enc_type = "App-Bound (v20)".to_string();
                v20_count += 1;
                preview = format!("{} (terkunci v20)", name);
            } else if enc_val.starts_with(b"v10") || enc_val.starts_with(b"v11") {
                enc_type = "DPAPI (v10)".to_string();
                if let Some(key) = master_key {
                    if let Ok(val) = decrypt_chromium_cookie_value(&enc_val, key) {
                        if !val.is_empty() {
                            is_decrypted = true;
                            decrypted_count += 1;
                            preview = format!("{} (didekripsi)", name);
                            cookies_map.insert(name.clone(), val);
                        }
                    }
                }
            } else {
                enc_type = "Legacy DPAPI".to_string();
                if let Ok(val) = decrypt_chromium_cookie_value(&enc_val, &[]) {
                    if !val.is_empty() {
                        is_decrypted = true;
                        decrypted_count += 1;
                        preview = format!("{} (didekripsi)", name);
                        cookies_map.insert(name.clone(), val);
                    }
                }
            }
        }

        diagnostic_items.push(CookieDiagnosticItem {
            name,
            host,
            encryption_type: enc_type,
            is_decrypted,
            is_auth_token: is_auth,
            preview,
        });
    }

    let mut header_parts = Vec::new();
    for (k, v) in &cookies_map {
        header_parts.push(format!("{}={}", k, v));
    }
    let cookie_header = header_parts.join("; ");

    let (success, status_code, message) = if decrypted_count > 0 {
        let auth_str = if has_auth_token {
            " (Token autentikasi ditemukan)"
        } else {
            ""
        };
        (
            true,
            "READY".to_string(),
            format!(
                "Berhasil mengekstrak {} cookies via DPAPI{}!",
                decrypted_count, auth_str
            ),
        )
    } else if v20_count > 0 {
        if provider == "deepseek" {
            (
                false,
                "NO_COOKIES".to_string(),
                format!(
                    "Sesi login DeepSeek tidak ditemukan di LocalStorage {}. Pastikan Anda telah login ke https://chat.deepseek.com di browser tersebut.",
                    profile.browser_name
                ),
            )
        } else {
            (
                false,
                "APP_BOUND".to_string(),
                format!(
                    "Ditemukan {} cookies domain untuk {}, namun terenkripsi proteksi Chromium 127+ App-Bound Encryption (v20). Gunakan browser Firefox atau Edge versi non-v20 untuk ekstraksi langsung.",
                    total_found,
                    provider.to_uppercase()
                ),
            )
        }
    } else if total_found == 0 {
        (
            false,
            "NO_COOKIES".to_string(),
            format!(
                "Tidak ditemukan cookies untuk {} pada profil {}. Pastikan Anda telah login ke akun Anda di browser.",
                provider.to_uppercase(),
                profile.profile_name
            ),
        )
    } else {
        (
            false,
            "DECRYPT_FAILED".to_string(),
            format!(
                "Ditemukan {} cookies di database, namun dekripsi master key DPAPI gagal.",
                total_found
            ),
        )
    };

    let statsig_seed = cookies_map.get("statsig_seed")
        .or_else(|| cookies_map.get("x-statsig-seed"))
        .cloned();
    let statsig_hex = cookies_map.get("statsig_hex")
        .or_else(|| cookies_map.get("x-statsig-id"))
        .or_else(|| cookies_map.get("x-statsig-hex"))
        .cloned();

    CookieSyncResult {
        success,
        provider: provider.to_string(),
        browser_name: profile.browser_name.clone(),
        profile_name: profile.profile_name.clone(),
        total_found,
        decrypted_count,
        v20_count,
        has_auth_token,
        status_code,
        message,
        cookie_items: diagnostic_items,
        cookie_header,
        statsig_seed,
        statsig_hex,
        account_label: None,
        cookies_map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_browsers_includes_edge_and_chrome() {
        let browsers = scan_available_browsers();
        println!("Detected browsers on system: {} profiles found", browsers.len());
        for b in &browsers {
            println!(" - [{}] {} ({}) => Path: {}", b.browser_type, b.browser_name, b.profile_name, b.cookies_path);
        }
        assert!(!browsers.is_empty(), "Should detect at least one browser on system");
        
        // Check if Chrome or Edge is detected
        let has_chromium = browsers.iter().any(|b| b.browser_type == "chrome" || b.browser_type == "edge");
        assert!(has_chromium, "Should detect Chrome or Edge on host system");
    }

    #[test]
    fn test_edge_diagnostic_extraction() {
        let browsers = scan_available_browsers();
        let edge_profile = browsers.iter().find(|b| b.browser_type == "edge");
        if let Some(profile) = edge_profile {
            println!("Testing diagnostic extraction on Edge profile: {}", profile.profile_name);
            println!("Cookies path: {}", profile.cookies_path);

            for p in &["deepseek", "grok", "gemini", "chatgpt", "claude"] {
                let diag = diagnose_cookie_extraction(profile, p);
                println!(
                    "Edge [{}] => status={}, found={}, decrypted={}, v20={}, has_auth={}, msg={}", 
                    p, diag.status_code, diag.total_found, diag.decrypted_count, diag.v20_count, diag.has_auth_token, diag.message
                );
                for item in &diag.cookie_items {
                    println!("    - Cookie: {} (host: {}, enc: {}, auth: {})", item.name, item.host, item.encryption_type, item.is_auth_token);
                }
            }
        } else {
            println!("Microsoft Edge not present in this test environment, skipping Edge-specific check");
        }
    }

    #[test]
    fn test_chrome_diagnostic_extraction() {
        let browsers = scan_available_browsers();
        let chrome_profile = browsers.iter().find(|b| b.browser_type == "chrome");
        if let Some(profile) = chrome_profile {
            println!("Testing diagnostic extraction on Chrome profile: {}", profile.profile_name);
            println!("Cookies path: {}", profile.cookies_path);

            for p in &["deepseek", "grok", "gemini", "chatgpt", "claude"] {
                let diag = diagnose_cookie_extraction(profile, p);
                println!(
                    "Chrome [{}] => status={}, found={}, decrypted={}, v20={}, has_auth={}, msg={}",
                    p, diag.status_code, diag.total_found, diag.decrypted_count, diag.v20_count, diag.has_auth_token, diag.message
                );
                for item in &diag.cookie_items {
                    println!("    - Cookie: {} (host: {}, enc: {}, auth: {})", item.name, item.host, item.encryption_type, item.is_auth_token);
                }
            }
        } else {
            println!("Google Chrome not present in this test environment, skipping Chrome-specific check");
        }
    }

    #[test]
    fn test_verify_deepseek_token_live() {
        // Token sintetik tidak valid harus ditolak server (40003)
        let dead_token = "SYNTHETIC_EXPIRED_TEST_TOKEN_00000000000000000000000000000000000";
        let res_dead = verify_deepseek_token(dead_token);
        println!("Dead token verification result: {:?}", res_dead);
        match res_dead {
            TokenVerifyStatus::Invalid(msg) => {
                assert!(msg.contains("40003") || msg.contains("menolak"));
            }
            TokenVerifyStatus::NetworkError(e) => {
                println!("Network error during test: {}", e);
            }
            other => panic!("Dead token should be Invalid or NetworkError, got: {:?}", other),
        }
    }
}


