use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{watch, Mutex};

use crate::config::Config;
use crate::deepseek::auth::{Account, AccountPool, AccountSpec};
use crate::deepseek::client::DeepSeekClient;
use crate::error::AppError;

static TMP_NONCE: AtomicU64 = AtomicU64::new(0);

/// Path to the sibling backup copy of `accounts.json` (`<path>.bak`).
pub fn backup_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.bak"))
}

fn write_file_atomic(target: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }
    }
    let nonce = TMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = PathBuf::from(format!(
        "{}.tmp.{}.{}",
        target.display(),
        std::process::id(),
        nonce
    ));
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| format!("create temp {}: {e}", tmp.display()))?;
        if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("write/sync temp {}: {e}", tmp.display()));
        }
    }

    let mut last_err = None;
    for attempt in 0..4u8 {
        match std::fs::rename(&tmp, target) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600));
                }
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(format!(
        "rename to {}: {}",
        target.display(),
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into())
    ))
}

fn load_accounts_with_fallback(path: &str) -> Vec<AccountSpec> {
    if let Ok(text) = std::fs::read_to_string(path) {
        if !text.trim().is_empty() {
            match serde_json::from_str::<Vec<AccountSpec>>(&text) {
                Ok(list) => return list,
                Err(e) => tracing::warn!("{path}: invalid JSON ({e}), trying backup"),
            }
        } else {
            tracing::warn!("{path}: empty file, trying backup");
        }
    }

    let bak = backup_path(path);
    if let Ok(bak_text) = std::fs::read_to_string(&bak) {
        if !bak_text.trim().is_empty() {
            match serde_json::from_str::<Vec<AccountSpec>>(&bak_text) {
                Ok(list) => {
                    if !list.is_empty() {
                        tracing::warn!(
                            "recovered {} account(s) from {}",
                            list.len(),
                            bak.display()
                        );
                        let _ = write_file_atomic(Path::new(path), bak_text.as_bytes());
                    }
                    return list;
                }
                Err(e) => tracing::warn!("{}: invalid backup JSON ({e})", bak.display()),
            }
        }
    }

    Vec::new()
}

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

        for spec in load_accounts_with_fallback(&cfg.accounts_file) {
            pool.add(spec);
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

    /// Atomically writes the account list to DS_ACCOUNTS_FILE and a `.bak`
    /// fallback copy. Contains secrets — the file is gitignored.
    pub async fn persist_accounts(&self) -> Result<(), String> {
        let specs = self.pool.specs();
        let path = self.cfg.accounts_file.clone();
        let json = serde_json::to_string_pretty(&specs)
            .map_err(|e| format!("cannot serialize accounts: {e}"))?;
        let has_accounts = !specs.is_empty();

        tokio::task::spawn_blocking(move || {
            let primary = PathBuf::from(&path);
            let bak = backup_path(&path);

            // Preserve prior healthy state in `.bak` before replacing primary when saving non-empty pool.
            if has_accounts {
                if let Ok(prev) = std::fs::read_to_string(&primary) {
                    if !prev.trim().is_empty()
                        && serde_json::from_str::<Vec<AccountSpec>>(&prev)
                            .map(|v| !v.is_empty())
                            .unwrap_or(false)
                    {
                        let _ = write_file_atomic(&bak, prev.as_bytes());
                    }
                }
            }

            write_file_atomic(&primary, json.as_bytes())?;
            write_file_atomic(&bak, json.as_bytes())?;
            Ok(())
        })
        .await
        .map_err(|e| format!("persist task panicked: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_accounts_path(tag: &str) -> PathBuf {
        let n = TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "api_ai_state_test_{tag}_{}_{}.json",
            std::process::id(),
            n
        ))
    }

    fn base_cfg(path: &Path) -> Arc<Config> {
        Arc::new(Config {
            host: "127.0.0.1".into(),
            port: 8787,
            api_keys: Vec::new(),
            admin_key: None,
            upstream: "https://chat.deepseek.com".into(),
            user_agent: "ua".into(),
            app_version: "1".into(),
            client_version: "1".into(),
            completion_path: "/api/v0/chat/completion".into(),
            pow_enabled: false,
            reuse_session: false,
            delete_session: true,
            accounts_file: path.to_string_lossy().into_owned(),
            default_model: "deepseek-chat".into(),
        })
    }

    #[tokio::test]
    async fn persist_accounts_writes_atomically_and_updates_backup() {
        let primary = temp_accounts_path("atomic");
        let bak = backup_path(&primary.to_string_lossy());
        let st = AppState::new(base_cfg(&primary)).await.unwrap();
        st.pool.add(AccountSpec {
            name: "chrome:Default".into(),
            cookie: "ds_session=abc".into(),
            token: Some("tok-atomic-123".into()),
            source: "chrome".into(),
            profile: "Default".into(),
            email: "a@example.com".into(),
        });
        st.persist_accounts().await.expect("atomic persist succeeds");

        assert!(primary.is_file());
        assert!(bak.is_file());
        let p_json: Vec<AccountSpec> =
            serde_json::from_str(&std::fs::read_to_string(&primary).unwrap()).unwrap();
        let b_json: Vec<AccountSpec> =
            serde_json::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(p_json.len(), 1);
        assert_eq!(b_json.len(), 1);
        assert_eq!(p_json[0].token.as_deref(), Some("tok-atomic-123"));

        let _ = std::fs::remove_file(&primary);
        let _ = std::fs::remove_file(&bak);
    }

    #[tokio::test]
    async fn load_accounts_recovers_from_backup_when_primary_corrupt_or_empty() {
        let primary = temp_accounts_path("recover");
        let bak = backup_path(&primary.to_string_lossy());
        let cfg = base_cfg(&primary);

        {
            let st = AppState::new(cfg.clone()).await.unwrap();
            st.pool.add(AccountSpec {
                name: "edge:Default".into(),
                cookie: String::new(),
                token: Some("tok-recover-999".into()),
                source: "edge".into(),
                profile: "Personal".into(),
                email: "b@example.com".into(),
            });
            st.persist_accounts().await.unwrap();
        }

        // 1) Corrupt truncated 0-byte primary
        std::fs::write(&primary, "").unwrap();
        let recovered1 = AppState::new(cfg.clone()).await.unwrap();
        assert_eq!(recovered1.pool.len(), 1);
        assert_eq!(recovered1.pool.snapshot()[0].name, "edge:Default");
        assert!(std::fs::read_to_string(&primary)
            .unwrap()
            .contains("tok-recover-999"));

        // 2) Malformed JSON primary
        std::fs::write(&primary, "{corrupt json[").unwrap();
        let recovered2 = AppState::new(cfg).await.unwrap();
        assert_eq!(recovered2.pool.len(), 1);

        let _ = std::fs::remove_file(&primary);
        let _ = std::fs::remove_file(&bak);
    }

    #[tokio::test]
    async fn explicit_empty_persist_clears_backup_so_deleted_keys_do_not_resurrect() {
        let primary = temp_accounts_path("clear");
        let bak = backup_path(&primary.to_string_lossy());
        let cfg = base_cfg(&primary);

        let st = AppState::new(cfg.clone()).await.unwrap();
        st.pool.add(AccountSpec {
            name: "chrome:Default".into(),
            cookie: String::new(),
            token: Some("tok-temp-1".into()),
            source: "chrome".into(),
            profile: "Default".into(),
            email: "c@example.com".into(),
        });
        st.persist_accounts().await.unwrap();
        assert_eq!(st.pool.len(), 1);

        // User explicitly removes all keys -> both primary & backup become `[]`
        assert!(st.pool.remove("chrome:Default"));
        st.persist_accounts().await.unwrap();

        let reloaded = AppState::new(cfg).await.unwrap();
        assert_eq!(reloaded.pool.len(), 0);

        let _ = std::fs::remove_file(&primary);
        let _ = std::fs::remove_file(&bak);
    }
}
