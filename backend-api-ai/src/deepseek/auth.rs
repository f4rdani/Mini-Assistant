use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::util::random_hex;

/// One upstream account = one browser session (cookie + user token).
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct AccountSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub email: String,
}

/// Never print the cookie or token.
impl std::fmt::Debug for AccountSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSpec")
            .field("name", &self.name)
            .field("source", &self.source)
            .field("email", &self.email)
            .field("has_cookie", &!self.cookie.is_empty())
            .field("has_token", &self.token.is_some())
            .finish()
    }
}

fn infer_source(name: &str, explicit: &str) -> String {
    let s = explicit.trim();
    if !s.is_empty() {
        return s.to_ascii_lowercase();
    }
    let lower = name.trim().to_ascii_lowercase();
    if lower.starts_with("chrome:") || lower == "chrome" {
        "chrome".to_string()
    } else if lower.starts_with("edge:") || lower == "edge" {
        "edge".to_string()
    } else if lower.starts_with("brave:") || lower == "brave" {
        "brave".to_string()
    } else if lower.starts_with("firefox:") || lower == "firefox" {
        "firefox".to_string()
    } else if lower == "manual" {
        "manual".to_string()
    } else if lower == "env" {
        "env".to_string()
    } else {
        String::new()
    }
}

pub struct Account {
    pub name: String,
    pub cookie: String,
    pub token: Option<String>,
    pub source: String,
    pub profile: String,
    pub email: RwLock<String>,
    pub device_id: String,
    pub busy: AtomicBool,
    pub last_error: Mutex<Option<String>>,
    pub cooldown_until: Mutex<Option<Instant>>,
}

impl Account {
    pub fn new(spec: AccountSpec, idx: usize) -> Self {
        let name = if spec.name.trim().is_empty() {
            format!("account-{idx}")
        } else {
            spec.name.trim().to_string()
        };
        let token = spec
            .token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .or_else(|| extract_token_from_cookie(&spec.cookie));
        let source = infer_source(&name, &spec.source);
        Self {
            name,
            cookie: spec.cookie.trim().to_string(),
            token,
            source,
            profile: spec.profile.trim().to_string(),
            email: RwLock::new(spec.email.trim().to_string()),
            device_id: random_hex(16),
            busy: AtomicBool::new(false),
            last_error: Mutex::new(None),
            cooldown_until: Mutex::new(None),
        }
    }

    pub fn email_str(&self) -> String {
        self.email.read().unwrap().clone()
    }

    pub fn set_email(&self, email: &str) {
        let trimmed = email.trim();
        if !trimmed.is_empty() {
            *self.email.write().unwrap() = trimmed.to_string();
        }
    }

    pub fn to_spec(&self) -> AccountSpec {
        AccountSpec {
            name: self.name.clone(),
            cookie: self.cookie.clone(),
            token: self.token.clone(),
            source: self.source.clone(),
            profile: self.profile.clone(),
            email: self.email_str(),
        }
    }

    pub fn cooldown_remaining_secs(&self) -> u64 {
        match *self.cooldown_until.lock().unwrap() {
            Some(t) => {
                let now = Instant::now();
                if t > now {
                    t.duration_since(now).as_secs().max(1)
                } else {
                    0
                }
            }
            None => 0,
        }
    }

    pub fn cooldown_ok(&self) -> bool {
        self.cooldown_remaining_secs() == 0
    }

    pub fn available(&self) -> bool {
        self.cooldown_ok() && !self.busy.load(Ordering::Relaxed)
    }

    pub fn set_cooldown(&self, secs: u64) {
        *self.cooldown_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(secs));
    }

    pub fn set_error(&self, msg: impl Into<String>) {
        *self.last_error.lock().unwrap() = Some(msg.into());
    }

    pub fn clear_error(&self) {
        *self.last_error.lock().unwrap() = None;
        *self.cooldown_until.lock().unwrap() = None;
    }

    pub fn info(&self) -> serde_json::Value {
        let cooldown_secs = self.cooldown_remaining_secs();
        json!({
            "name": self.name,
            "source": self.source,
            "profile": self.profile,
            "email": self.email_str(),
            "has_cookie": !self.cookie.is_empty(),
            "has_token": self.token.is_some(),
            "busy": self.busy.load(Ordering::Relaxed),
            "cooling_down": cooldown_secs > 0,
            "cooldown_secs": cooldown_secs,
            "available": self.available(),
            "last_error": self.last_error.lock().unwrap().clone(),
        })
    }
}

fn extract_token_from_cookie(cookie: &str) -> Option<String> {
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("userToken=") {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

pub struct AccountPool {
    accounts: RwLock<Vec<Arc<Account>>>,
    next_id: AtomicUsize,
}

impl Default for AccountPool {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountPool {
    pub fn new() -> Self {
        Self {
            accounts: RwLock::new(Vec::new()),
            next_id: AtomicUsize::new(0),
        }
    }

    pub fn add(&self, spec: AccountSpec) -> String {
        let mut w = self.accounts.write().unwrap();
        let auto = spec.name.trim().is_empty();
        let mut idx = self.next_id.fetch_add(1, Ordering::Relaxed);
        if auto {
            while w.iter().any(|a| a.name == format!("account-{idx}")) {
                idx = self.next_id.fetch_add(1, Ordering::Relaxed);
            }
        }
        let acc = Arc::new(Account::new(spec, idx));
        let name = acc.name.clone();
        if let Some(pos) = w.iter().position(|a| a.name == name) {
            w[pos] = acc;
        } else if let Some(ref tok) = acc.token {
            // Avoid duplicate fallback keys when two slots hold the exact same DeepSeek token.
            if let Some(dup_pos) = w.iter().position(|a| a.token.as_deref() == Some(tok.as_str())) {
                w[dup_pos] = acc;
            } else {
                w.push(acc);
            }
        } else {
            w.push(acc);
        }
        name
    }

    pub fn set_primary(&self, name: &str) -> bool {
        let mut w = self.accounts.write().unwrap();
        if let Some(pos) = w.iter().position(|a| a.name == name) {
            if pos > 0 {
                let target = w.remove(pos);
                w.insert(0, target);
            }
            true
        } else {
            false
        }
    }

    pub fn set_email(&self, name: &str, email: &str) -> bool {
        let trimmed = email.trim();
        if trimmed.is_empty() {
            return false;
        }
        let list = self.snapshot();
        if let Some(acc) = list.iter().find(|a| a.name == name) {
            acc.set_email(trimmed);
            true
        } else {
            false
        }
    }

    pub fn remove(&self, name: &str) -> bool {
        let mut w = self.accounts.write().unwrap();
        let before = w.len();
        w.retain(|a| a.name != name);
        w.len() != before
    }

    pub fn snapshot(&self) -> Vec<Arc<Account>> {
        self.accounts.read().unwrap().clone()
    }

    pub fn specs(&self) -> Vec<AccountSpec> {
        self.snapshot().iter().map(|a| a.to_spec()).collect()
    }

    /// Priority-first acquisition: prefers Key 1 (index 0) whenever it is healthy
    /// and idle, automatically falling back to Key 2, Key 3, ... only when earlier
    /// keys are busy, in cooldown, or already tried during this request.
    pub fn try_acquire_excluding(&self, excluded: &[String]) -> Option<Arc<Account>> {
        let list = self.snapshot();
        for acc in list {
            if excluded.iter().any(|e| e == &acc.name) {
                continue;
            }
            if !acc.cooldown_ok() {
                continue;
            }
            if acc
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(acc);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.accounts.read().unwrap().len()
    }

    pub fn info(&self) -> Vec<serde_json::Value> {
        self.snapshot()
            .iter()
            .enumerate()
            .map(|(idx, a)| {
                let mut v = a.info();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("priority".to_string(), json!(idx + 1));
                }
                v
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, token: &str, source: &str, email: &str) -> AccountSpec {
        AccountSpec {
            name: name.to_string(),
            cookie: String::new(),
            token: Some(token.to_string()),
            source: source.to_string(),
            profile: String::new(),
            email: email.to_string(),
        }
    }

    #[test]
    fn priority_acquisition_prefers_first_account_until_unavailable() {
        let pool = AccountPool::new();
        pool.add(spec("chrome:Default", "tok-chrome-1234567890", "chrome", "a@gmail.com"));
        pool.add(spec("edge:Default", "tok-edge-0987654321", "edge", "b@gmail.com"));

        // Repeated calls stay on Key 1 (Chrome) while healthy and idle.
        for _ in 0..3 {
            let acc = pool.try_acquire_excluding(&[]).expect("primary available");
            assert_eq!(acc.name, "chrome:Default");
            acc.busy.store(false, Ordering::Relaxed);
        }

        // Excluding Key 1 falls back to Key 2 (Edge).
        let excluded = vec!["chrome:Default".to_string()];
        let fb = pool.try_acquire_excluding(&excluded).expect("fallback available");
        assert_eq!(fb.name, "edge:Default");
        fb.busy.store(false, Ordering::Relaxed);

        // Putting Key 1 in cooldown automatically acquires Key 2 without exclusion list.
        let primary = pool.snapshot()[0].clone();
        primary.set_cooldown(60);
        assert!(primary.cooldown_remaining_secs() > 0);
        let fb2 = pool.try_acquire_excluding(&[]).expect("fallback during cooldown");
        assert_eq!(fb2.name, "edge:Default");
        fb2.busy.store(false, Ordering::Relaxed);

        // Clearing error restores Key 1 immediately.
        primary.clear_error();
        let restored = pool.try_acquire_excluding(&[]).expect("restored primary");
        assert_eq!(restored.name, "chrome:Default");
    }

    #[test]
    fn add_dedups_identical_token_and_set_primary_reorders() {
        let pool = AccountPool::new();
        pool.add(spec("chrome:Default", "tok-a", "chrome", "a@gmail.com"));
        pool.add(spec("edge:Default", "tok-b", "edge", "b@gmail.com"));
        assert_eq!(pool.len(), 2);

        // Identical token in another slot updates without creating a duplicate.
        pool.add(spec("manual", "tok-a", "manual", "a@gmail.com"));
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.snapshot()[0].name, "manual");

        assert!(pool.set_primary("edge:Default"));
        assert_eq!(pool.snapshot()[0].name, "edge:Default");
        assert_eq!(pool.info()[0]["priority"], 1);
        assert_eq!(pool.info()[1]["priority"], 2);
    }

    #[test]
    fn legacy_accounts_json_without_meta_deserializes_cleanly() {
        let legacy = r#"[{"name":"main","cookie":"","token":"tok-legacy-123"}]"#;
        let list: Vec<AccountSpec> = serde_json::from_str(legacy).unwrap();
        assert_eq!(list.len(), 1);
        let acc = Account::new(list[0].clone(), 0);
        assert_eq!(acc.name, "main");
        assert_eq!(acc.source, "");
        assert_eq!(acc.email_str(), "");
    }
}
