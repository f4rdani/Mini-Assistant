use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Optional bearer keys for OUR api. Empty = open access.
    pub api_keys: Vec<String>,
    /// Key required for /admin/*. Empty = allowed only on a loopback bind.
    pub admin_key: Option<String>,
    pub upstream: String,
    pub user_agent: String,
    pub app_version: String,
    pub client_version: String,
    pub completion_path: String,
    pub pow_enabled: bool,
    pub reuse_session: bool,
    /// Delete the chat session after each request so web history stays clean.
    pub delete_session: bool,
    pub accounts_file: String,
    pub default_model: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: env::var("PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8787),
            api_keys: env::var("API_KEYS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            admin_key: env::var("ADMIN_KEY").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
            upstream: env::var("DS_BASE_URL")
                .unwrap_or_else(|_| "https://chat.deepseek.com".into())
                .trim_end_matches('/')
                .to_string(),
            user_agent: env::var("DS_USER_AGENT").unwrap_or_else(|_| {
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".into()
            }),
            app_version: env::var("DS_APP_VERSION").unwrap_or_else(|_| "20241129.1".into()),
            client_version: env::var("DS_CLIENT_VERSION").unwrap_or_else(|_| "1.0.0-always".into()),
            completion_path: env::var("DS_COMPLETION_PATH")
                .unwrap_or_else(|_| "/api/v0/chat/completion".into()),
            pow_enabled: env::var("DS_POW")
                .map(|v| v != "off" && v != "false" && v != "0")
                .unwrap_or(true),
            reuse_session: env::var("DS_REUSE_SESSION")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            delete_session: env::var("DS_DELETE_SESSION")
                .map(|v| v != "false" && v != "0" && v != "off")
                .unwrap_or(true),
            accounts_file: env::var("DS_ACCOUNTS_FILE").unwrap_or_else(|_| "accounts.json".into()),
            default_model: env::var("DEFAULT_MODEL").unwrap_or_else(|_| "deepseek-chat".into()),
        })
    }

    pub fn auth_required(&self) -> bool {
        !self.api_keys.is_empty()
    }

    pub fn is_loopback(&self) -> bool {
        matches!(self.host.trim(), "127.0.0.1" | "localhost" | "::1")
    }
}
