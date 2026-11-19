pub mod admin;
pub mod openai;

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::config::Config;
use crate::error::AppError;
use crate::state::AppState;

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                .unwrap_or(v)
        })
        .unwrap_or("")
}

/// Optional auth for our own API surface (API_KEYS env).
pub fn authorize(cfg: &Config, headers: &HeaderMap) -> Result<(), AppError> {
    if !cfg.auth_required() {
        return Ok(());
    }
    let token = bearer(headers);
    let xkey = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if cfg
        .api_keys
        .iter()
        .any(|k| k == token || (!xkey.is_empty() && k == xkey))
    {
        Ok(())
    } else {
        Err(AppError::Unauthorized("invalid api key".into()))
    }
}

/// /admin/* mutates credentials, so it needs its own key unless we are bound to
/// loopback only.
pub fn authorize_admin(cfg: &Config, headers: &HeaderMap) -> Result<(), AppError> {
    if let Some(key) = &cfg.admin_key {
        let token = bearer(headers);
        let xkey = headers
            .get("x-admin-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if token == key || xkey == key {
            return Ok(());
        }
        return Err(AppError::Unauthorized("invalid admin key".into()));
    }
    if cfg.is_loopback() {
        return Ok(());
    }
    Err(AppError::Unauthorized(
        "admin API disabled on a non-loopback bind: set ADMIN_KEY".into(),
    ))
}

pub async fn health(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "upstream": st.cfg.upstream,
        "accounts": st.pool.len(),
        "pow": st.cfg.pow_enabled,
    }))
}
