use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::Stream;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::multipart;
use serde_json::json;

use crate::config::Config;
use crate::deepseek::auth::Account;
use crate::deepseek::pow::{self, PowChallenge};
use crate::util::truncate;

pub type ByteStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;

/// Attachments fetched from a client supplied url are capped hard.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

fn upstream_code_error(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let code = v.get("code")?.as_i64()?;
    if code == 0 {
        return None;
    }
    let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
    let hint = match code {
        40002 => " — token missing, set localStorage userToken (see README)",
        40003 => " — token invalid/expired, re-copy cookie + userToken",
        40001 => " — request rejected by upstream",
        _ => "",
    };
    Some(format!("upstream {code}: {msg}{hint}"))
}

pub struct DeepSeekClient {
    http: reqwest::Client,
    /// Separate client for user supplied urls: no redirects, hard timeout.
    media: reqwest::Client,
    cfg: Arc<Config>,
}

impl DeepSeekClient {
    pub fn new(cfg: Arc<Config>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        let media = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, media, cfg })
    }

    fn base_headers(&self, acc: &Account) -> anyhow::Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert("accept", HeaderValue::from_static("*/*"));
        h.insert(
            "accept-language",
            HeaderValue::from_static("en-US,en;q=0.9,id;q=0.8"),
        );
        if let Ok(v) = HeaderValue::from_str(&self.cfg.upstream) {
            h.insert("origin", v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("{}/", self.cfg.upstream)) {
            h.insert("referer", v);
        }
        if let Ok(v) = HeaderValue::from_str(&self.cfg.user_agent) {
            h.insert("user-agent", v);
        }
        if let Ok(v) = HeaderValue::from_str(&self.cfg.app_version) {
            h.insert("x-app-version", v);
        }
        if let Ok(v) = HeaderValue::from_str(&self.cfg.client_version) {
            h.insert("x-client-version", v);
        }
        h.insert("x-client-locale", HeaderValue::from_static("en_US"));
        h.insert("x-client-platform", HeaderValue::from_static("web"));
        h.insert(
            "x-client-bundle-id",
            HeaderValue::from_static("com.deepseek.chat"),
        );
        if let Ok(v) = HeaderValue::from_str(&acc.device_id) {
            h.insert("x-device-id", v);
        }
        if !acc.cookie.is_empty() {
            let v = HeaderValue::from_str(&acc.cookie).context(
                "cookie contains characters that are not allowed in an HTTP header (newline or non-ASCII?)",
            )?;
            h.insert("cookie", v);
        }
        if let Some(token) = &acc.token {
            let v = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("token contains characters that are not allowed in an HTTP header")?;
            h.insert("authorization", v);
        }
        Ok(h)
    }

    async fn pow_header(&self, acc: &Account, target_path: &str) -> anyhow::Result<Option<String>> {
        if !self.cfg.pow_enabled {
            return Ok(None);
        }
        let url = format!("{}/api/v0/chat/create_pow_challenge", self.cfg.upstream);
        let mut h = self.base_headers(acc)?;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let resp = self
            .http
            .post(url)
            .headers(h)
            .timeout(Duration::from_secs(30))
            .json(&json!({ "target_path": target_path }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("create_pow_challenge {status}: {}", truncate(&text, 400));
        }
        if let Some(e) = upstream_code_error(&text) {
            anyhow::bail!("create_pow_challenge: {e}");
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).context("create_pow_challenge: invalid json")?;
        // shape: {"data":{"biz_data":{"challenge":{...}}}}
        let biz = v
            .pointer("/data/biz_data/challenge")
            .or_else(|| v.pointer("/data/biz_data"))
            .ok_or_else(|| anyhow::anyhow!("create_pow_challenge: {}", truncate(&text, 400)))?;
        let challenge: PowChallenge = serde_json::from_value(biz.clone())?;

        // interpreting the wasm is CPU bound: keep it off the async worker
        let answer = tokio::task::spawn_blocking(move || pow::solve(&challenge))
            .await
            .context("pow task panicked")??;
        Ok(Some(answer))
    }

    pub async fn create_session(&self, acc: &Account) -> anyhow::Result<String> {
        let url = format!("{}/api/v0/chat_session/create", self.cfg.upstream);
        let mut h = self.base_headers(acc)?;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let resp = self
            .http
            .post(url)
            .headers(h)
            .timeout(Duration::from_secs(30))
            .json(&json!({ "character_id": null }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        // shape: {"data":{"biz_data":{"chat_session":{"id":"..."}}}}
        let id = v
            .pointer("/data/biz_data/chat_session/id")
            .or_else(|| v.pointer("/data/biz_data/id"))
            .and_then(|x| x.as_str());
        if let Some(id) = id {
            return Ok(id.to_string());
        }
        if let Some(e) = upstream_code_error(&text) {
            anyhow::bail!("create_session: {e}");
        }
        anyhow::bail!("create_session {status}: {}", truncate(&text, 400))
    }

    /// Open the SSE stream for one completion.
    pub async fn completion(
        &self,
        acc: &Account,
        body: &serde_json::Value,
    ) -> anyhow::Result<ByteStream> {
        let url = format!("{}{}", self.cfg.upstream, self.cfg.completion_path);
        let mut h = self.base_headers(acc)?;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert("accept", HeaderValue::from_static("*/*"));
        if let Some(pow) = self.pow_header(acc, &self.cfg.completion_path).await? {
            h.insert(
                "x-ds-pow-response",
                HeaderValue::from_str(&pow).context("pow header not ascii")?,
            );
        }

        let resp = self.http.post(url).headers(h).json(body).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("completion {status}: {}", truncate(&text, 800));
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !content_type.contains("event-stream") {
            let text = resp.text().await.unwrap_or_default();
            if let Some(e) = upstream_code_error(&text) {
                anyhow::bail!("{e}");
            }
            anyhow::bail!(
                "unexpected content-type '{content_type}': {}",
                truncate(&text, 400)
            );
        }

        Ok(Box::pin(resp.bytes_stream()))
    }

    /// Upload a file (image / pdf / doc) and return the DeepSeek file id.
    pub async fn upload_file(
        &self,
        acc: &Account,
        filename: String,
        mime: String,
        data: Vec<u8>,
    ) -> anyhow::Result<String> {
        if data.len() > MAX_ATTACHMENT_BYTES {
            anyhow::bail!(
                "file too large: {} bytes (max {MAX_ATTACHMENT_BYTES})",
                data.len()
            );
        }
        let url = format!("{}/api/v0/file/upload_file", self.cfg.upstream);
        let mut h = self.base_headers(acc)?;
        // the upload endpoint is pow-protected too
        if let Some(pow) = self.pow_header(acc, "/api/v0/file/upload_file").await? {
            h.insert(
                "x-ds-pow-response",
                HeaderValue::from_str(&pow).context("pow header not ascii")?,
            );
        }

        let make_part = |data: &Vec<u8>| {
            multipart::Part::bytes(data.clone())
                .file_name(filename.clone())
                .mime_str(&mime)
                .unwrap_or_else(|_| multipart::Part::bytes(data.clone()).file_name(filename.clone()))
        };
        let form = multipart::Form::new().part("file", make_part(&data));

        let resp = self
            .http
            .post(url)
            .headers(h)
            .timeout(Duration::from_secs(120))
            .multipart(form)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if let Some(id) = v.pointer("/data/biz_data/id").and_then(|x| x.as_str()) {
            let state = v
                .pointer("/data/biz_data/status")
                .and_then(|x| x.as_str())
                .unwrap_or("SUCCESS");
            if !state.eq_ignore_ascii_case("SUCCESS") {
                // images/pdf are parsed asynchronously: PENDING -> PARSING -> SUCCESS
                self.wait_file_ready(acc, id).await?;
            }
            return Ok(id.to_string());
        }
        if let Some(e) = upstream_code_error(&text) {
            anyhow::bail!("upload_file: {e}");
        }
        anyhow::bail!("upload_file {status}: {}", truncate(&text, 400))
    }

    /// Delete a chat session so the account's web history stays clean.
    pub async fn delete_session(&self, acc: &Account, chat_session_id: &str) -> anyhow::Result<()> {
        let url = format!("{}/api/v0/chat_session/delete", self.cfg.upstream);
        let mut h = self.base_headers(acc)?;
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let resp = self
            .http
            .post(url)
            .headers(h)
            .timeout(Duration::from_secs(20))
            .json(&json!({ "chat_session_id": chat_session_id }))
            .send()
            .await?;
        let text = resp.text().await.unwrap_or_default();
        if let Some(e) = upstream_code_error(&text) {
            anyhow::bail!("delete_session: {e}");
        }
        Ok(())
    }

    /// Poll `/api/v0/file/fetch_files` until the upload finished parsing.
    async fn wait_file_ready(&self, acc: &Account, id: &str) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/v0/file/fetch_files?file_ids={}",
            self.cfg.upstream, id
        );
        for attempt in 0..60 {
            let h = self.base_headers(acc)?;
            let resp = self
                .http
                .get(&url)
                .headers(h)
                .timeout(Duration::from_secs(15))
                .send()
                .await?;
            let text = resp.text().await.unwrap_or_default();
            let v: serde_json::Value =
                serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            let state = v
                .pointer("/data/biz_data/files/0/status")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            match state {
                "SUCCESS" => return Ok(()),
                "FAILED" => anyhow::bail!("file processing failed upstream"),
                _ => {
                    if attempt == 59 {
                        anyhow::bail!("file still {state} after 36s");
                    }
                    tokio::time::sleep(Duration::from_millis(600)).await;
                }
            }
        }
        Ok(())
    }

    /// Fetch a client supplied url (image_url content part) -> (mime, bytes).
    /// Only http(s) is allowed, private/loopback targets are rejected and the
    /// body is capped, so this cannot be used to probe the local network.
    pub async fn download(&self, url: &str) -> anyhow::Result<(String, Vec<u8>)> {
        if let Some(rest) = url.strip_prefix("data:") {
            let (meta, payload) = rest
                .split_once(',')
                .ok_or_else(|| anyhow::anyhow!("invalid data url"))?;
            let mime = meta.split(';').next().unwrap_or("application/octet-stream");
            let bytes = if meta.contains(";base64") {
                base64::engine::general_purpose::STANDARD
                    .decode(payload.trim())
                    .context("invalid base64 data url")?
            } else {
                payload.as_bytes().to_vec()
            };
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                anyhow::bail!("attachment too large");
            }
            return Ok((mime.to_string(), bytes));
        }

        ensure_public_url(url).await?;

        let resp = self.media.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("download {status}: {url}");
        }
        let mime = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let mut out: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if out.len() + chunk.len() > MAX_ATTACHMENT_BYTES {
                anyhow::bail!("attachment too large (max {MAX_ATTACHMENT_BYTES} bytes)");
            }
            out.extend_from_slice(&chunk);
        }
        Ok((mime, out))
    }

    /// Queries `/api/v0/users/current` for this account's masked email/profile label.
    pub async fn fetch_user_email(&self, acc: &Account) -> Option<String> {
        let url = format!("{}/api/v0/users/current", self.cfg.upstream);
        let h = self.base_headers(acc).ok()?;
        let resp = self
            .http
            .get(url)
            .headers(h)
            .timeout(Duration::from_secs(6))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let text = resp.text().await.ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
            return None;
        }
        v.pointer("/data/biz_data/email")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                v.pointer("/data/biz_data/id_profile/name")
                    .and_then(|s| s.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_string)
    }
}

async fn ensure_public_url(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url).context("invalid attachment url")?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => anyhow::bail!("unsupported url scheme '{other}'"),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("attachment url has no host"))?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);

    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve {host}"))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if is_blocked_ip(addr.ip()) {
            anyhow::bail!("attachment url resolves to a private address");
        }
    }
    if !any {
        anyhow::bail!("cannot resolve {host}");
    }
    Ok(())
}

fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            // ::ffff:127.0.0.1 reaches the same host as 127.0.0.1; without this
            // the v4 rules are trivially bypassed by writing the address in
            // mapped form.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(std::net::IpAddr::V4(v4));
            }
            // fe80::/10 has no stable helper, check the first segment
            let link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
            v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() || link_local
        }
    }
}
