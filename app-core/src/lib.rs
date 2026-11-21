//! Mini Assistant core.
//!
//! This crate is the whole application backend with no UI framework and no
//! webview: cookie extraction (Chrome SQLite + DPAPI + App-Bound), local
//! SQLite storage, Win32 screen capture, global hotkeys, foreground-window
//! detection, process metrics, and a client for the local provider API.
//!
//! The Slint frontend links this crate directly (in-process, no IPC).

pub mod modules;
pub mod provider;

use std::sync::mpsc;
use std::sync::Mutex;

use base64::prelude::*;
use serde_json::Value;

pub use modules::clipboard::{image_png, sequence as clipboard_sequence};
pub use modules::cookie_extractor::{
    diagnose_cookie_extraction, scan_available_browsers, BrowserProfileInfo, CookieDiagnosticItem,
    CookieSyncResult,
};
pub use modules::hotkey::{
    parse_key_code, parse_modifier_code, set_translate_hotkey_callback,
    simulate_translate_hotkey, start_global_hotkey_listener, update_global_hotkey,
    update_translate_hotkey,
};
pub use modules::i18n::{current_lang, is_en, set_lang, tr, tr_for, AppLang};
pub use modules::screen_capture::{capture_screen_base64, capture_target_window_or_screen_base64};
pub use modules::storage::{
    is_portable_mode, portable_accounts_path, portable_cache_dir, resolve_data_dir, topic_label,
    ChatMessage, MessageRecord, StorageManager, StoredSearchSource, TopicItem,
};
pub use modules::system_metrics::{get_live_metrics, SystemMetrics};
pub use modules::window_detector::{detect_target_window, record_foreground_window, CurrentWindowInfo};
pub use provider::{
    AccountTestResult, ChatEvent, ChatRequest, Health, ProviderClient, ProviderConfig, Usage,
};
/// Provider-side message (role + content). Aliased so it does not clash with
/// the stored `ChatMessage` row type above.
pub use provider::ChatMessage as ChatTurn;

/// A provider the backend can serve, as offered to the UI.
#[derive(Debug, Clone)]
pub struct ProviderOption {
    pub id: String,
    pub label: String,
    /// Model id used when the user picks this provider.
    pub default_model: String,
}

/// Display name for an upstream family. Unknown families fall back to the
/// capitalised id, so a new backend provider shows up without a UI change.
fn provider_label(id: &str) -> String {
    match id {
        "deepseek" => "DeepSeek".to_string(),
        "openai" => "ChatGPT".to_string(),
        "anthropic" => "Claude".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => other.to_string(),
            }
        }
    }
}

/// Metadata and live pool state of a registered upstream account ("Key").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEntry {
    pub name: String,
    pub source: String,
    pub profile: String,
    pub email: String,
    pub priority: usize,
    pub available: bool,
    pub busy: bool,
    pub cooling_down: bool,
    pub cooldown_secs: u64,
    pub last_error: String,
}

/// Deterministic account slot name for a browser profile (`chrome:Default`, `edge:Default`, etc.).
pub fn account_slot_name(profile: &BrowserProfileInfo) -> String {
    format!("{}:{}", profile.browser_type, profile.profile_id)
}

/// Result of "take the session out of the browser and hand it to the API".
#[derive(Debug, Clone)]
pub struct ConnectReport {
    pub success: bool,
    pub browser: String,
    pub profile: String,
    pub status: String,
    pub message: String,
    pub token_chars: usize,
    /// Cookies that were actually readable (DPAPI v10 / plaintext).
    pub cookies_found: usize,
    /// Cookies present but sealed with Chrome's App-Bound key (v20).
    pub locked_v20: usize,
}

pub struct Core {
    provider: ProviderClient,
    storage: Mutex<StorageManager>,
    rt: tokio::runtime::Runtime,
}

impl Core {
    pub fn new(provider_cfg: ProviderConfig) -> Result<Self, String> {
        let provider = ProviderClient::new(provider_cfg)?;
        let storage = StorageManager::new()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("runtime: {e}"))?;
        Ok(Self { provider, storage: Mutex::new(storage), rt })
    }

    /// Exposes the shared Tokio runtime handle so an embedded API server can
    /// reuse the same lightweight worker threads instead of spawning a second runtime.
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    // ---------------------------------------------------------------- browser

    pub fn scan_browsers(&self) -> Vec<BrowserProfileInfo> {
        scan_available_browsers()
    }

    /// Reads the DeepSeek session from the browser and registers it on the
    /// provider API. This is the bridge: browser cookie -> API account.
    pub fn connect_deepseek(&self, profile: &BrowserProfileInfo) -> ConnectReport {
        let diag = diagnose_cookie_extraction(profile, "deepseek");
        #[cfg(debug_assertions)]
        eprintln!(
            "[vault] {} · {} cookie · {} terbaca · {} terkunci v20 · {}",
            diag.status_code, diag.total_found, diag.decrypted_count, diag.v20_count, diag.message
        );
        let token = diag
            .cookies_map
            .get("userToken")
            .cloned()
            .or_else(|| diag.cookies_map.get("d_token").cloned())
            .unwrap_or_default();

        let base = ConnectReport {
            success: false,
            browser: profile.browser_name.clone(),
            profile: profile.profile_name.clone(),
            status: diag.status_code.clone(),
            message: diag.message.clone(),
            token_chars: token.len(),
            cookies_found: diag.decrypted_count,
            locked_v20: diag.v20_count,
        };

        if !diag.success || token.trim().is_empty() {
            return ConnectReport {
                message: if !diag.message.is_empty() {
                    diag.message.clone()
                } else if is_en() {
                    format!(
                        "DeepSeek session not found in {} (profile '{}'). Status: {}. Please sign in to that profile first.",
                        profile.browser_name, profile.profile_name, diag.status_code
                    )
                } else {
                    format!(
                        "Sesi DeepSeek tidak ditemukan di {} (profil '{}'). Status: {}. Login dulu di profil itu.",
                        profile.browser_name, profile.profile_name, diag.status_code
                    )
                },
                ..base
            };
        }

        let verified_email = diag
            .account_label
            .clone()
            .filter(|s| !s.trim().is_empty() && s != "Offline")
            .or_else(|| {
                if let modules::cookie_extractor::TokenVerifyStatus::Valid(email) =
                    modules::cookie_extractor::verify_deepseek_token(token.trim())
                {
                    Some(email)
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let slot = account_slot_name(profile);
        let registered = self.rt.block_on(async {
            match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                self.provider.register_account_with_meta(
                    &slot,
                    token.trim(),
                    &diag.cookie_header,
                    &profile.browser_type,
                    &profile.profile_name,
                    &verified_email,
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(tr(
                    "provider API tidak merespons (timeout 15s)",
                    "provider API did not respond (15s timeout)",
                )
                .to_string()),
            }
        });
        match registered {
            Ok(()) => {
                if !verified_email.is_empty() {
                    let _ = self.save_setting("deepseek-account-email", &verified_email);
                }
                ConnectReport {
                    success: true,
                    status: "READY".to_string(),
                    message: if is_en() {
                        format!(
                            "DeepSeek session from {} (profile '{}') connected to provider API.",
                            profile.browser_name, profile.profile_name
                        )
                    } else {
                        format!(
                            "Sesi DeepSeek dari {} (profil '{}') terpasang ke provider API.",
                            profile.browser_name, profile.profile_name
                        )
                    },
                    ..base
                }
            }
            Err(e) => ConnectReport { message: e, ..base },
        }
    }

    /// Extracts and registers DeepSeek sessions across all detected browser profiles.
    pub fn connect_deepseek_all_browsers(&self) -> Vec<ConnectReport> {
        self.scan_browsers()
            .iter()
            .map(|p| self.connect_deepseek(p))
            .collect()
    }

    /// Startup background sync:
    /// - When the API pool is empty (and `allow_empty_sync` is true), imports
    ///   available DeepSeek sessions from Chrome/Edge automatically.
    /// - When accounts exist, performs a quick token validity check against DeepSeek;
    ///   only if DeepSeek explicitly rejects the stored token (`Invalid`, not offline),
    ///   silently refreshes fresh sessions from installed browsers.
    pub fn auto_sync_startup_sessions(&self, allow_empty_sync: bool) -> Option<Vec<ConnectReport>> {
        let current = self.deepseek_accounts();
        if current.is_empty() {
            if !allow_empty_sync {
                return None;
            }
            return Some(self.connect_deepseek_all_browsers());
        }
        if let Some(tok) = read_primary_saved_token() {
            if matches!(
                modules::cookie_extractor::verify_deepseek_token(&tok),
                modules::cookie_extractor::TokenVerifyStatus::Invalid(_)
            ) {
                let reports = self.connect_deepseek_all_browsers();
                if reports.iter().any(|r| r.success) {
                    return Some(reports);
                }
            }
        }
        None
    }

    // --------------------------------------------------------------- provider

    /// Health probe with a hard deadline: this runs on the UI thread at startup,
    /// so a dead provider API must not freeze the window for the full connect
    /// timeout.
    pub fn api_health(&self) -> Result<Health, String> {
        self.rt.block_on(async {
            match tokio::time::timeout(
                std::time::Duration::from_millis(1200),
                self.provider.health(),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err("provider API tidak merespons (timeout 1.2s)".to_string()),
            }
        })
    }

    /// Model list served by the provider API. The UI never hardcodes model
    /// names: whatever `/v1/models` returns is what the panels show.
    pub fn models(&self) -> Result<Vec<provider::ModelInfo>, String> {
        // Called on the UI thread while the window is being built: a provider
        // API that accepts the connection but never answers must not hold the
        // first frame hostage.
        self.rt.block_on(async {
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.provider.models(),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err("provider API tidak merespons (timeout 3s)".to_string()),
            }
        })
    }

    /// Providers the backend can actually serve, grouped by upstream family.
    /// Adding another provider later only requires configuring it on the API side.
    pub fn providers(&self) -> Result<Vec<ProviderOption>, String> {
        let mut out: Vec<ProviderOption> = Vec::new();
        for m in self.models()? {
            let family = m.owned_by.trim_end_matches("-web").to_string();
            match out.iter_mut().find(|p| p.id == family) {
                Some(p) => {
                    if m.id.ends_with("-chat") {
                        p.default_model = m.id.clone();
                    }
                }
                None => out.push(ProviderOption {
                    label: provider_label(&family),
                    id: family,
                    default_model: m.id.clone(),
                }),
            }
        }
        Ok(out)
    }

    /// Accounts currently loaded on the provider API (the cookie vault).
    pub fn accounts(&self) -> Result<Value, String> {
        self.rt.block_on(async {
            match tokio::time::timeout(
                std::time::Duration::from_millis(1800),
                self.provider.list_accounts(),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err("provider API tidak merespons (timeout 1.8s)".to_string()),
            }
        })
    }

    /// Structured list of active DeepSeek keys in priority order (`Key 1`, `Key 2`, ...).
    pub fn deepseek_accounts(&self) -> Vec<AccountEntry> {
        let Ok(v) = self.accounts() else {
            return Vec::new();
        };
        let Some(arr) = v.get("accounts").and_then(|a| a.as_array()) else {
            return Vec::new();
        };
        let fallback_email = self
            .get_setting("deepseek-account-email")
            .ok()
            .flatten()
            .unwrap_or_default();
        arr.iter()
            .enumerate()
            .map(|(idx, item)| {
                let name = item["name"].as_str().unwrap_or("").to_string();
                let raw_email = item["email"].as_str().unwrap_or("").trim().to_string();
                let email = if !raw_email.is_empty() {
                    raw_email
                } else if name == "main" && !fallback_email.is_empty() {
                    fallback_email.clone()
                } else if name.is_empty() {
                    format!("Akun {}", idx + 1)
                } else {
                    name.clone()
                };
                let source = item["source"].as_str().unwrap_or("").to_string();
                let priority = item["priority"]
                    .as_u64()
                    .map(|p| p as usize)
                    .unwrap_or(idx + 1);
                AccountEntry {
                    name,
                    source,
                    profile: item["profile"].as_str().unwrap_or("").to_string(),
                    email,
                    priority,
                    available: item["available"].as_bool().unwrap_or(true),
                    busy: item["busy"].as_bool().unwrap_or(false),
                    cooling_down: item["cooling_down"].as_bool().unwrap_or(false),
                    cooldown_secs: item["cooldown_secs"].as_u64().unwrap_or(0),
                    last_error: item["last_error"].as_str().unwrap_or("").to_string(),
                }
            })
            .collect()
    }

    pub fn remove_account(&self, name: &str) -> Result<(), String> {
        self.rt.block_on(self.provider.remove_account(name))
    }

    pub fn set_primary_account(&self, name: &str) -> Result<(), String> {
        self.rt.block_on(self.provider.set_primary_account(name))
    }

    /// Manual vault entry: paste a token and hand it to the provider API.
    /// This is the second cookie path, next to browser extraction.
    pub fn register_token(&self, name: &str, token: &str) -> Result<(), String> {
        let trimmed = token.trim();
        let slot = if name.trim().is_empty() || name == "main" {
            "manual"
        } else {
            name.trim()
        };
        let verified_email = if let modules::cookie_extractor::TokenVerifyStatus::Valid(email) =
            modules::cookie_extractor::verify_deepseek_token(trimmed)
        {
            let _ = self.save_setting("deepseek-account-email", &email);
            email
        } else {
            String::new()
        };
        self.rt.block_on(self.provider.register_account_with_meta(
            slot,
            trimmed,
            "",
            "manual",
            "Manual Token",
            &verified_email,
        ))
    }

    /// Returns the verified account email for DeepSeek, if available.
    pub fn get_deepseek_email(&self) -> Option<String> {
        let list = self.deepseek_accounts();
        if let Some(first) = list.first() {
            if !first.email.trim().is_empty() && first.email != first.name {
                return Some(first.email.clone());
            }
        }
        if let Ok(Some(saved)) = self.get_setting("deepseek-account-email") {
            if !saved.trim().is_empty() {
                return Some(saved);
            }
        }
        let candidates = [
            portable_accounts_path(),
            std::path::PathBuf::from("accounts.json"),
            std::path::PathBuf::from("backend-api-ai/accounts.json"),
            std::path::PathBuf::from("../backend-api-ai/accounts.json"),
        ];
        for path in &candidates {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(arr) = val.as_array() {
                        for acc in arr {
                            if let Some(email) = acc.get("email").and_then(|e| e.as_str()).filter(|s| !s.trim().is_empty()) {
                                return Some(email.to_string());
                            }
                            let tok = acc.get("token").and_then(|t| t.as_str())
                                .or_else(|| acc.get("cookie").and_then(|c| c.as_str()));
                            if let Some(token) = tok {
                                if let modules::cookie_extractor::TokenVerifyStatus::Valid(email) =
                                    modules::cookie_extractor::verify_deepseek_token(token)
                                {
                                    let _ = self.save_setting("deepseek-account-email", &email);
                                    return Some(email);
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// One non-streaming round trip, timed. Used by the diagnostics page to
    /// prove the whole chain (API -> DeepSeek -> answer) really works.
    pub fn test_completion(&self, model: &str) -> Result<(String, u128), String> {
        let started = std::time::Instant::now();
        let req = ChatRequest::new(vec![provider::ChatMessage::user(
            "Balas satu kata: OK",
        )])
        .model(model);
        let (text, _) = self.rt.block_on(self.provider.chat(req))?;
        Ok((text, started.elapsed().as_millis()))
    }

    /// Probes one specific DeepSeek account by slot name (`chrome:Default`,
    /// `edge:Default`, `manual`, ...) without cross-key fallback.
    pub fn test_deepseek_account(&self, name: &str) -> Result<AccountTestResult, String> {
        self.rt.block_on(self.provider.test_account(name))
    }

    /// Streams a completion. Events are forwarded onto a std channel so the UI
    /// thread can poll them from a Slint timer without an async runtime.
    pub fn chat_stream(&self, req: ChatRequest) -> mpsc::Receiver<ChatEvent> {
        self.chat_stream_with_attachment(req, None)
    }

    /// Streams a completion, uploading an attached file in the background if provided.
    /// This never blocks the calling thread, so UI sends are instant (0ms latency).
    pub fn chat_stream_with_attachment(
        &self,
        req: ChatRequest,
        attachment_path: Option<std::path::PathBuf>,
    ) -> mpsc::Receiver<ChatEvent> {
        self.chat_stream_with_attachments(req, attachment_path.into_iter().collect())
    }

    /// Streams a completion, uploading multiple attached files in the background if provided.
    /// This never blocks the calling thread, so UI sends are instant (0ms latency).
    pub fn chat_stream_with_attachments(
        &self,
        req: ChatRequest,
        attachment_paths: Vec<std::path::PathBuf>,
    ) -> mpsc::Receiver<ChatEvent> {
        let (tx, rx) = mpsc::channel();
        let _enter = self.rt.enter();
        let provider = self.provider.clone();
        self.rt.spawn(async move {
            let mut refreshed_auth = false;
            'attempts: for attempt in 0..2 {
                let mut req_attempt = req.clone();
                let mut uploaded_ids: Vec<String> = Vec::new();
                let mut uploaded_parts: Vec<serde_json::Value> = Vec::new();
                for path in &attachment_paths {
                    match provider.upload_file(path).await {
                        Ok(id) => {
                            uploaded_ids.push(id.clone());
                            uploaded_parts.push(serde_json::json!({
                                "type": "file",
                                "file": { "file_id": id }
                            }));
                        }
                        Err(e) => {
                            if attempt == 0
                                && !refreshed_auth
                                && is_auth_or_empty_pool_error(&e)
                                && refresh_browser_sessions_async(&provider).await > 0
                            {
                                refreshed_auth = true;
                                continue 'attempts;
                            }
                            let prefix = tr("Lampiran gagal diunggah", "Attachment upload failed");
                            let _ = tx.send(ChatEvent::Error(format!("{prefix}: {e}")));
                            return;
                        }
                    }
                }

                if !uploaded_parts.is_empty() {
                    if let Some(last) = req_attempt.messages.last_mut() {
                        match &mut last.content {
                            serde_json::Value::Array(arr) => {
                                arr.extend(uploaded_parts);
                            }
                            serde_json::Value::String(s) => {
                                let mut parts = vec![serde_json::json!({ "type": "text", "text": s.clone() })];
                                parts.extend(uploaded_parts);
                                last.content = serde_json::Value::Array(parts);
                            }
                            _ => {}
                        }
                    }
                }

                let mut inner = provider.chat_stream(req_attempt);
                let mut saw_output = false;
                let mut flushed_uploads = false;
                let mut retry_needed = false;
                while let Some(ev) = inner.recv().await {
                    match &ev {
                        ChatEvent::Delta(_) | ChatEvent::Reasoning(_) | ChatEvent::Search(_) => {
                            saw_output = true;
                            if !flushed_uploads {
                                flushed_uploads = true;
                                for id in &uploaded_ids {
                                    let _ = tx.send(ChatEvent::Uploaded(id.clone()));
                                }
                            }
                        }
                        ChatEvent::Done(_) => {
                            if !flushed_uploads {
                                flushed_uploads = true;
                                for id in &uploaded_ids {
                                    let _ = tx.send(ChatEvent::Uploaded(id.clone()));
                                }
                            }
                        }
                        ChatEvent::Error(ref err) if attempt == 0 && !saw_output => {
                            if !refreshed_auth
                                && is_auth_or_empty_pool_error(err)
                                && refresh_browser_sessions_async(&provider).await > 0
                            {
                                refreshed_auth = true;
                                retry_needed = true;
                                break;
                            }
                            if !attachment_paths.is_empty() {
                                // Backend cooled down failed key; re-upload attachment once onto fallback key.
                                retry_needed = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                    let last = matches!(ev, ChatEvent::Done(_) | ChatEvent::Error(_));
                    if tx.send(ev).is_err() {
                        return;
                    }
                    if last {
                        return;
                    }
                }
                if !retry_needed {
                    return;
                }
            }
        });
        rx
    }

    // ------------------------------------------------------------ attachments

    /// Uploads any file to the provider API and returns its id. The id can be
    /// reused on later turns, so an image is uploaded once, not every message.
    pub fn upload_file(&self, path: &std::path::Path) -> Result<String, String> {
        self.rt.block_on(self.provider.upload_file(path))
    }

    /// Screenshot written to the app's cache folder so the UI can preview it
    /// and reuse the same path when the message is sent.
    pub fn capture_to_file(&self, target_window_only: bool) -> Result<std::path::PathBuf, String> {
        static SHOT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let raw = self.capture_screen(target_window_only)?;
        // The capture layer returns a full data URL ("data:image/jpeg;base64,...").
        let b64 = raw.rsplit(',').next().unwrap_or(&raw);
        let bytes = BASE64_STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| format!("decode screenshot: {e}"))?;
        let dir = portable_cache_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let seq = SHOT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("shot-{stamp}-{seq}.jpg"));
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }

    // ---------------------------------------------------------------- storage

    pub fn topics(&self) -> Result<Vec<TopicItem>, String> {
        self.storage.lock().map_err(|e| e.to_string())?.get_topics()
    }

    pub fn topic_counts(&self) -> Result<Vec<(String, i64)>, String> {
        self.storage.lock().map_err(|e| e.to_string())?.topic_counts()
    }

    pub fn messages(&self, topic: &str) -> Result<Vec<ChatMessage>, String> {
        self.storage.lock().map_err(|e| e.to_string())?.get_messages(topic)
    }

    pub fn save_message(
        &self,
        topic: &str,
        category: &str,
        role: &str,
        content: &str,
        image_url: Option<&str>,
    ) -> Result<(), String> {
        self.storage
            .lock()
            .map_err(|e| e.to_string())?
            .save_message(topic, category, role, content, image_url)
    }

    pub fn save_message_record(&self, rec: &MessageRecord<'_>) -> Result<(), String> {
        self.storage
            .lock()
            .map_err(|e| e.to_string())?
            .save_message_record(rec)
    }

    pub fn delete_topic(&self, topic: &str) -> Result<(), String> {
        self.storage.lock().map_err(|e| e.to_string())?.delete_topic(topic)
    }

    /// Creates an empty session so it shows up in the history list before the
    /// first message is written.
    pub fn ensure_topic(&self, name: &str, category: &str) -> Result<(), String> {
        self.storage
            .lock()
            .map_err(|e| e.to_string())?
            .ensure_topic(name, category)
    }

    pub fn clear_history(&self) -> Result<(), String> {
        self.storage.lock().map_err(|e| e.to_string())?.clear_all_history()
    }

    pub fn save_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.storage.lock().map_err(|e| e.to_string())?.save_setting(key, value)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        self.storage.lock().map_err(|e| e.to_string())?.get_setting(key)
    }

    // ----------------------------------------------------------------- system

    /// Full-screen grab. When target_window_only is true the recorded game
    /// window is preferred, falling back to the whole desktop.
    pub fn capture_screen(&self, target_window_only: bool) -> Result<String, String> {
        if target_window_only {
            capture_target_window_or_screen_base64(None).0
        } else {
            capture_screen_base64()
        }
    }

    pub fn current_window(&self) -> Option<CurrentWindowInfo> {
        detect_target_window(None)
    }

    pub fn metrics(&self) -> SystemMetrics {
        get_live_metrics()
    }

    /// Hides this window from screen captures (WDA_EXCLUDEFROMCAPTURE) so the
    /// assistant does not appear in its own screenshots.
    #[cfg(windows)]
    pub fn set_capture_visible(&self, hwnd: isize, visible: bool) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetWindowDisplayAffinity(
                hwnd as _,
                if visible { 0x0000_0000 } else { 0x0000_0011 },
            );
        }
    }

    /// Background thread that remembers which window (usually the game) was in
    /// front, so screenshots can target it later.
    pub fn start_window_tracker(&self) {
        modules::window_detector::start_window_tracker();
    }

    /// Test hook: fires the hotkey listener as if Alt+Q was pressed.
    pub fn simulate_hotkey(&self) -> bool {
        modules::hotkey::simulate_hotkey()
    }

    /// Test hook: fires the translate hotkey listener as if Alt+T was pressed.
    pub fn simulate_translate_hotkey(&self) -> bool {
        modules::hotkey::simulate_translate_hotkey()
    }

    /// Fires on the returned channel every time the global hotkey is pressed.
    pub fn start_hotkey(&self) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        start_global_hotkey_listener(Box::new(move || {
            let _ = tx.send(());
        }));
        rx
    }

    /// Fires on the returned channel every time the translate hotkey is pressed.
    pub fn start_translate_hotkey(&self) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        set_translate_hotkey_callback(Box::new(move || {
            let _ = tx.send(());
        }));
        rx
    }

    /// Stops background hotkeys, cleans transient screenshot cache, and optionally
    /// shuts down an external `backend-api-ai` daemon cleanly.
    pub fn shutdown_system_with_api(&self, stop_external_api: bool) {
        modules::hotkey::stop_global_hotkeys();
        cleanup_transient_cache();
        if !stop_external_api {
            return;
        }
        let res = self.rt.block_on(self.provider.shutdown());
        if let Err((Some(404 | 405), _)) = res {
            #[cfg(windows)]
            if self.provider.is_loopback_base() {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                let _ = std::process::Command::new("taskkill")
                    .args(["/IM", "api-ai.exe", "/T", "/F"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
        }
    }

    /// Stops background hotkeys and shuts down the local `backend-api-ai` daemon cleanly.
    pub fn shutdown_system(&self) {
        self.shutdown_system_with_api(true);
    }
}

fn cleanup_transient_cache() {
    let dir = portable_cache_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Decodes base64 payloads (standard or URL-safe, tolerating embedded whitespace)
/// for inline `data:image/...;base64,...` responses.
pub fn decode_base64_bytes(b64: &str) -> Result<Vec<u8>, String> {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};
    use base64::Engine as _;
    let compact: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| URL_SAFE.decode(compact.as_bytes()))
        .map_err(|e| format!("decode base64: {e}"))
}

/// Downloads an image asset from HTTP/HTTPS with strict timeout and 10 MB size cap.
pub fn fetch_image_bytes(url: &str) -> Result<(Vec<u8>, Option<String>), String> {
    use std::io::Read as _;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent("MiniAssistant/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    const MAX_BYTES: u64 = 10 * 1024 * 1024;
    if let Some(len) = resp.content_length() {
        if len > MAX_BYTES {
            return Err("gambar terlalu besar (>10 MB)".to_string());
        }
    }
    let mut buf = Vec::new();
    resp.take(MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_BYTES {
        return Err("gambar terlalu besar (>10 MB)".to_string());
    }
    Ok((buf, content_type))
}

/// Returns `true` when an upstream or provider-API error indicates empty account
/// pool or expired/rejected DeepSeek session credentials suitable for auto-refresh.
pub fn is_auth_or_empty_pool_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    [
        "no deepseek account",
        "no account",
        "40002",
        "40003",
        "401",
        "invalid token",
        "token invalid",
        "expired",
        "authorization failed",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

fn read_primary_saved_token() -> Option<String> {
    let mut candidates = Vec::new();
    if let Ok(env_path) = std::env::var("DS_ACCOUNTS_FILE") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            candidates.push(std::path::PathBuf::from(trimmed));
        }
    }
    candidates.push(portable_accounts_path());

    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        for acc in arr {
            if let Some(tok) = acc
                .get("token")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(tok.to_string());
            }
            if let Some(cookie) = acc.get("cookie").and_then(|v| v.as_str()) {
                for part in cookie.split(';') {
                    if let Some(rest) = part.trim().strip_prefix("userToken=") {
                        let t = rest.trim();
                        if !t.is_empty() {
                            return Some(t.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

struct ExtractedBrowserSession {
    slot: String,
    token: String,
    cookie_header: String,
    browser_type: String,
    profile_name: String,
    verified_email: String,
}

async fn refresh_browser_sessions_async(provider: &ProviderClient) -> usize {
    let extracted = tokio::task::spawn_blocking(|| {
        let mut out = Vec::new();
        for profile in modules::cookie_extractor::scan_available_browsers() {
            let diag = modules::cookie_extractor::diagnose_cookie_extraction(&profile, "deepseek");
            if !diag.success {
                continue;
            }
            let token = diag
                .cookies_map
                .get("userToken")
                .cloned()
                .or_else(|| diag.cookies_map.get("d_token").cloned())
                .unwrap_or_default();
            let trimmed = token.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            let verified_email = diag
                .account_label
                .clone()
                .filter(|s| !s.trim().is_empty() && s != "Offline")
                .or_else(|| {
                    if let modules::cookie_extractor::TokenVerifyStatus::Valid(email) =
                        modules::cookie_extractor::verify_deepseek_token(&trimmed)
                    {
                        Some(email)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            out.push(ExtractedBrowserSession {
                slot: account_slot_name(&profile),
                token: trimmed,
                cookie_header: diag.cookie_header,
                browser_type: profile.browser_type,
                profile_name: profile.profile_name,
                verified_email,
            });
        }
        out
    })
    .await
    .unwrap_or_default();

    let mut registered = 0usize;
    for sess in extracted {
        if let Ok(Ok(())) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.register_account_with_meta(
                &sess.slot,
                &sess.token,
                &sess.cookie_header,
                &sess.browser_type,
                &sess.profile_name,
                &sess.verified_email,
            ),
        )
        .await
        {
            registered += 1;
        }
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_slot_name_separates_chrome_and_edge() {
        let chrome = BrowserProfileInfo {
            browser_type: "chrome".into(),
            browser_name: "Google Chrome".into(),
            profile_id: "Default".into(),
            profile_name: "Google Chrome (Chrome Anda)".into(),
            cookies_path: "".into(),
        };
        let edge = BrowserProfileInfo {
            browser_type: "edge".into(),
            browser_name: "Microsoft Edge".into(),
            profile_id: "Default".into(),
            profile_name: "Microsoft Edge (Personal)".into(),
            cookies_path: "".into(),
        };
        assert_eq!(account_slot_name(&chrome), "chrome:Default");
        assert_eq!(account_slot_name(&edge), "edge:Default");
        assert_ne!(account_slot_name(&chrome), account_slot_name(&edge));
    }

    #[test]
    fn detects_auth_and_empty_pool_errors_for_auto_refresh() {
        assert!(is_auth_or_empty_pool_error(
            "No DeepSeek account configured. POST /admin/accounts"
        ));
        assert!(is_auth_or_empty_pool_error(
            "upstream 40003: token invalid/expired"
        ));
        assert!(is_auth_or_empty_pool_error("HTTP 401 Unauthorized"));
        assert!(is_auth_or_empty_pool_error("upload HTTP 401: expired session"));
        assert!(!is_auth_or_empty_pool_error("rate limit exceeded 429"));
        assert!(!is_auth_or_empty_pool_error("upstream timed out"));
    }
}
