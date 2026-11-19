//! Thin client for the local provider API (`backend-api-ai`).
//!
//! The provider API speaks the OpenAI wire format, so this module is the only
//! place in the app that knows about HTTP/SSE. Everything else stays sync.

use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn default_model() -> String {
    "deepseek-chat".to_string()
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub admin_key: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8787".to_string(),
            api_key: None,
            admin_key: None,
        }
    }
}

/// One turn. Content is a JSON value because OpenAI accepts either a plain
/// string or an array of parts (text / image_url / file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Value::String(content.into()) }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Value::String(content.into()) }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Value::String(content.into()) }
    }

    /// User turn with attachments. Parts are OpenAI content parts.
    pub fn user_parts(text: &str, parts: Vec<Value>) -> Self {
        let mut all = vec![json!({ "type": "text", "text": text })];
        all.extend(parts);
        Self { role: "user".into(), content: Value::Array(all) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    #[serde(default = "default_model")]
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// DeepSeek "Pikir Mendalam". Non-standard OpenAI fields, understood by
    /// backend-api-ai and mapped to the upstream thinking/search switches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<bool>,
}

impl ChatRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            model: default_model(),
            messages,
            stream: false,
            temperature: None,
            thinking: None,
            search: None,
        }
    }

    /// Thinking + web search on every turn, so answers stay current.
    pub fn with_reasoning_and_search(mut self) -> Self {
        self.thinking = Some(true);
        self.search = Some(true);
        self
    }
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// One row of `/v1/models`.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// Upstream family (`deepseek-web`), the unit the UI lets the user pick.
    pub owned_by: String,
}

/// Incremental events produced while a streamed answer is generated.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    Uploaded(String),
    Delta(String),
    Reasoning(String),
    Search(Vec<Value>),
    Done(Option<Usage>),
    Error(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub status: String,
    #[serde(default)]
    pub accounts: u64,
    #[serde(default)]
    pub pow: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountTestResult {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderClient {
    http: reqwest::Client,
    cfg: ProviderConfig,
}

impl ProviderClient {
    pub fn new(cfg: ProviderConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self { http, cfg })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    fn user_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cfg.api_key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }

    fn admin_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cfg.admin_key {
            Some(k) => rb.header("x-admin-key", k),
            None => rb,
        }
    }

    pub async fn health(&self) -> Result<Health, String> {
        let resp = self
            .http
            .get(self.url("/health"))
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi ({}): {e}", self.cfg.base_url))?;
        if !resp.status().is_success() {
            return Err(format!("health HTTP {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    /// Registers (or replaces) an account on the provider API. This is the last
    /// step of cookie extraction: the token we pulled out of the browser is what
    /// the API will use for the next request.
    pub async fn register_account(&self, name: &str, token: &str, cookie: &str) -> Result<(), String> {
        self.register_account_with_meta(name, token, cookie, "", "", "")
            .await
    }

    pub async fn register_account_with_meta(
        &self,
        name: &str,
        token: &str,
        cookie: &str,
        source: &str,
        profile: &str,
        email: &str,
    ) -> Result<(), String> {
        let resp = self
            .admin_auth(self.http.post(self.url("/admin/accounts")))
            .json(&json!({
                "name": name,
                "token": token,
                "cookie": cookie,
                "source": source,
                "profile": profile,
                "email": email,
            }))
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi: {e}"))?;
        if resp.status().is_success() {
            return Ok(());
        }
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("gagal mendaftarkan akun (HTTP {code}): {body}"))
    }

    pub async fn remove_account(&self, name: &str) -> Result<(), String> {
        let seg = encode_path_segment(name);
        let resp = self
            .admin_auth(self.http.delete(self.url(&format!("/admin/accounts/{seg}"))))
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi: {e}"))?;
        if resp.status().is_success() {
            return Ok(());
        }
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("gagal menghapus akun (HTTP {code}): {body}"))
    }

    pub async fn set_primary_account(&self, name: &str) -> Result<(), String> {
        let seg = encode_path_segment(name);
        let resp = self
            .admin_auth(self.http.post(self.url(&format!("/admin/accounts/{seg}/primary"))))
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi: {e}"))?;
        if resp.status().is_success() {
            return Ok(());
        }
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("gagal mengatur akun utama (HTTP {code}): {body}"))
    }

    pub async fn test_account(&self, name: &str) -> Result<AccountTestResult, String> {
        let seg = encode_path_segment(name);
        let resp = self
            .admin_auth(self.http.post(self.url(&format!("/admin/accounts/{seg}/test"))))
            .timeout(Duration::from_secs(35))
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi: {e}"))?;
        let code = resp.status();
        if !code.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("uji key HTTP {code}: {body}"));
        }
        resp.json::<AccountTestResult>()
            .await
            .map_err(|e| format!("respons uji key tidak valid: {e}"))
    }

    /// Uploads a file to the provider API and returns its id, which can then be
    /// referenced as a file part inside a message.
    pub async fn upload_file(&self, path: &std::path::Path) -> Result<String, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("baca file: {e}"))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload.bin")
            .to_string();
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name)
            .mime_str(mime_for(path))
            .map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = self
            .user_auth(self.http.post(self.url("/v1/files")))
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("upload gagal: {e}"))?;
        let code = resp.status();
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        if !code.is_success() {
            return Err(error_message(&body).unwrap_or_else(|| format!("upload HTTP {code}")));
        }
        body["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "upload: respons tanpa id".to_string())
    }

    pub async fn list_accounts(&self) -> Result<Value, String> {
        let resp = self
            .admin_auth(self.http.get(self.url("/admin/accounts")))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("admin HTTP {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    pub fn is_loopback_base(&self) -> bool {
        let lower = self.cfg.base_url.to_ascii_lowercase();
        lower.contains("://127.0.0.1") || lower.contains("://localhost") || lower.contains("://[::1]")
    }

    /// Requests graceful shutdown of the local `backend-api-ai` process.
    /// Returns `Ok(true)` if handled cleanly, or `Err((status_opt, msg))` when unavailable.
    pub async fn shutdown(&self) -> Result<(), (Option<u16>, String)> {
        let resp = self
            .admin_auth(self.http.post(self.url("/admin/shutdown")))
            .timeout(Duration::from_millis(1200))
            .send()
            .await
            .map_err(|e| (None, e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        Err((Some(status.as_u16()), format!("shutdown HTTP {status}")))
    }

    /// `/v1/models` as served by the provider API.
    ///
    /// `owned_by` groups the variants of one upstream: deepseek-chat,
    /// -reasoner, -search and -vision are all `deepseek-web`, which is what the
    /// UI offers as a single selectable provider.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, String> {
        let resp = self
            .user_auth(self.http.get(self.url("/v1/models")))
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("models HTTP {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let list = body["data"].as_array().cloned().unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|m| {
                Some(ModelInfo {
                    id: m["id"].as_str()?.to_string(),
                    owned_by: m["owned_by"].as_str().unwrap_or("unknown").to_string(),
                })
            })
            .collect())
    }

    /// Non-streaming completion.
    pub async fn chat(&self, req: ChatRequest) -> Result<(String, Option<Usage>), String> {
        let mut req = req;
        req.stream = false;
        let resp = self
            .user_auth(self.http.post(self.url("/v1/chat/completions")))
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("provider API tidak bisa dihubungi: {e}"))?;
        let code = resp.status();
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        if !code.is_success() {
            return Err(error_message(&body).unwrap_or_else(|| format!("HTTP {code}")));
        }
        let text = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let usage = body.get("usage").and_then(|u| serde_json::from_value(u.clone()).ok());
        Ok((text, usage))
    }

    /// Streaming completion. Returns immediately; events arrive on the channel.
    pub fn chat_stream(&self, req: ChatRequest) -> mpsc::UnboundedReceiver<ChatEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let http = self.http.clone();
        let url = self.url("/v1/chat/completions");
        let key = self.cfg.api_key.clone();
        let mut req = req;
        req.stream = true;

        tokio::spawn(async move {
            let mut rb = http.post(&url).json(&req);
            if let Some(k) = key {
                rb = rb.bearer_auth(k);
            }
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Error(format!("provider API tidak bisa dihubungi: {e}")));
                    return;
                }
            };
            let code = resp.status();
            if !code.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| error_message(&v))
                    .unwrap_or_else(|| format!("HTTP {code}"));
                let _ = tx.send(ChatEvent::Error(msg));
                return;
            }

            let mut stream = resp.bytes_stream();
            // Bytes, not String: a multi-byte character (Indonesian accents,
            // emoji, CJK) can be split across two TCP chunks, and decoding each
            // chunk on its own would replace half of it with U+FFFD. Lines are
            // only decoded once the terminating newline has arrived.
            let mut buf: Vec<u8> = Vec::new();
            let mut usage: Option<Usage> = None;

            while let Some(chunk) = stream.next().await {
                // The UI drops the receiver when it starts a new request or the
                // HUD goes away. Stop pulling tokens nobody will read instead
                // of keeping the upstream connection busy until the end.
                if tx.is_closed() {
                    return;
                }
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(ChatEvent::Error(format!("koneksi terputus: {e}")));
                        return;
                    }
                };
                buf.extend_from_slice(&bytes);

                while let Some(idx) = buf.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&buf[..idx]).to_string();
                    buf.drain(..=idx);
                    let line = line.trim();
                    let Some(payload) = line.strip_prefix("data:") else { continue };
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    if payload == "[DONE]" {
                        let _ = tx.send(ChatEvent::Done(usage));
                        return;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(payload) else { continue };
                    if let Some(msg) = error_message(&v) {
                        let _ = tx.send(ChatEvent::Error(msg));
                        return;
                    }
                    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                        usage = serde_json::from_value(u.clone()).ok();
                    }
                    let Some(delta) = v["choices"][0].get("delta") else { continue };
                    if let Some(r) = delta["reasoning_content"].as_str().filter(|s| !s.is_empty()) {
                        let _ = tx.send(ChatEvent::Reasoning(r.to_string()));
                    }
                    if let Some(s) = delta["search_results"].as_array() {
                        let _ = tx.send(ChatEvent::Search(s.clone()));
                    }
                    if let Some(c) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                        let _ = tx.send(ChatEvent::Delta(c.to_string()));
                    }
                }
            }
            let _ = tx.send(ChatEvent::Done(usage));
        });

        rx
    }
}

fn encode_path_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.as_bytes() {
        let ch = *b as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn error_message(v: &Value) -> Option<String> {
    v.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

/// Minimal extension -> mime map. Anything unknown is treated as binary.
pub fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        _ => "application/octet-stream",
    }
}

/// True for the formats DeepSeek accepts as vision input.
pub fn is_image(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif") | Some("bmp")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only_turn_is_a_string() {
        let m = ChatMessage::user("halo");
        assert_eq!(m.content, Value::String("halo".into()));
    }

    #[test]
    fn turn_with_parts_is_an_array_with_text_first() {
        let m = ChatMessage::user_parts(
            "apa ini?",
            vec![json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AA" } })],
        );
        let arr = m.content.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
    }

    #[test]
    fn image_detection_and_mime() {
        use std::path::Path;
        assert!(is_image(Path::new("a.PNG")));
        assert!(is_image(Path::new("shot.jpeg")));
        assert!(!is_image(Path::new("laporan.pdf")));
        assert_eq!(mime_for(Path::new("x.webp")), "image/webp");
        assert_eq!(mime_for(Path::new("x.unknown")), "application/octet-stream");
    }

    #[test]
    fn account_test_result_deserializes_ok_and_fail() {
        let ok: AccountTestResult = serde_json::from_str(
            r#"{"ok":true,"name":"chrome:Default","latency_ms":385,"reply":"OK"}"#,
        )
        .unwrap();
        assert!(ok.ok);
        assert_eq!(ok.latency_ms, Some(385));
        assert_eq!(ok.reply.as_deref(), Some("OK"));

        let fail: AccountTestResult = serde_json::from_str(
            r#"{"ok":false,"name":"edge:Default","latency_ms":null,"error":"upstream 40003: token invalid"}"#,
        )
        .unwrap();
        assert!(!fail.ok);
        assert!(fail.error.as_deref().unwrap().contains("40003"));
    }
}
