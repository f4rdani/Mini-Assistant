use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{watch, Mutex};

use crate::config::Config;
use crate::deepseek::auth::{Account, AccountPool, AccountSpec};
use crate::deepseek::client::DeepSeekClient;
use crate::error::AppError;

pub struct AppState {
    pub cfg: Arc<Config>,
    pub pool: AccountPool,
    pub ds: DeepSeekClient,
    pub sessions: Mutex<HashMap<String, String>>,
    pub shutdown_tx: watch::Sender<bool>,
}

impl AppState {
    pub async fn new(cfg: Arc<Config>) -> anyhow::Result<Self> {
        let ds = DeepSeekClient::new(cfg.clone())?;
        let pool = AccountPool::new();

        if let Ok(text) = std::fs::read_to_string(&cfg.accounts_file) {
            match serde_json::from_str::<Vec<AccountSpec>>(&text) {
                Ok(list) => {
                    for spec in list {
                        pool.add(spec);
                    }
                }
                Err(e) => tracing::warn!("{}: {e}", cfg.accounts_file),
            }
        }

        // a token alone is enough — the web API accepts Bearer auth without cookies
        let env_cookie = std::env::var("DS_COOKIE").unwrap_or_default();
        let env_token = std::env::var("DS_TOKEN").ok();
        let has_token = env_token
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false);
        if !env_cookie.trim().is_empty() || has_token {
            pool.add(AccountSpec {
                name: "env".into(),
                cookie: env_cookie,
                token: env_token,
                source: "env".into(),
                profile: String::new(),
                email: String::new(),
            });
        }

        if pool.len() == 0 {
            tracing::warn!(
                "no account loaded — POST /admin/accounts or fill {} before calling /v1/chat/completions",
                cfg.accounts_file
            );
        } else {
            tracing::info!("loaded {} account(s)", pool.len());
        }

        let (shutdown_tx, _) = watch::channel(false);

        Ok(Self {
            cfg,
            pool,
            ds,
            sessions: Mutex::new(HashMap::new()),
            shutdown_tx,
        })
    }

    /// DeepSeek keeps conversation state server side, so every request needs a
    /// chat_session_id. Default: a fresh one per request (stateless like OpenAI).
    pub async fn session_id(&self, acc: &Account) -> Result<String, AppError> {
        if !self.cfg.reuse_session {
            return self.ds.create_session(acc).await.map_err(AppError::from);
        }
        if let Some(id) = self.sessions.lock().await.get(&acc.name).cloned() {
            return Ok(id);
        }
        let id = self.ds.create_session(acc).await.map_err(AppError::from)?;
        self.sessions
            .lock()
            .await
            .insert(acc.name.clone(), id.clone());
        Ok(id)
    }

    pub async fn forget_session(&self, name: &str) {
        self.sessions.lock().await.remove(name);
    }

    /// Write the account list back to DS_ACCOUNTS_FILE. Contains secrets — the
    /// file is gitignored, keep it that way.
    pub async fn persist_accounts(&self) {
        let specs = self.pool.specs();
        let path = self.cfg.accounts_file.clone();
        let json = match serde_json::to_string_pretty(&specs) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("cannot serialize accounts: {e}");
                return;
            }
        };
        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }
        if let Err(e) = tokio::fs::write(&path, json).await {
            tracing::warn!("cannot write {path}: {e}");
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                {
                    tracing::warn!("cannot chmod 600 {path}: {e}");
                }
            }
        }
    }
}
