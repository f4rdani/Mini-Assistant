use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::api::authorize_admin;
use crate::deepseek::auth::{Account, AccountSpec};
use crate::error::AppError;
use crate::state::AppState;

/// Credentials end up in HTTP headers, so they must be single-line printable
/// ASCII. Reject anything else at the door instead of storing a broken account.
fn check_credential(label: &str, v: &str) -> Result<(), AppError> {
    if v.chars().any(|c| c.is_control()) {
        return Err(AppError::BadRequest(format!(
            "{label} contains control characters (newline/tab) — copy it again, it should be one line"
        )));
    }
    if !v.is_ascii() {
        return Err(AppError::BadRequest(format!(
            "{label} contains non-ASCII characters — copy it again, it should be plain text"
        )));
    }
    Ok(())
}

pub async fn backfill_missing_emails(st: &Arc<AppState>) {
    let missing: Vec<_> = st
        .pool
        .snapshot()
        .into_iter()
        .filter(|a| a.email_str().is_empty())
        .collect();
    if missing.is_empty() {
        return;
    }
    let futs = missing.iter().map(|acc| {
        let ds = &st.ds;
        async move { (acc.name.clone(), ds.fetch_user_email(acc).await) }
    });
    let results = futures_util::future::join_all(futs).await;
    let mut updated = false;
    for (name, email_opt) in results {
        if let Some(email) = email_opt {
            if st.pool.set_email(&name, &email) {
                updated = true;
            }
        }
    }
    if updated {
        let _ = st.persist_accounts().await;
    }
}

pub async fn list(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize_admin(&st.cfg, &headers)?;
    if st.pool.snapshot().iter().any(|a| a.email_str().is_empty()) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1200),
            backfill_missing_emails(&st),
        )
        .await;
    }
    Ok(Json(json!({ "accounts": st.pool.info() })))
}

pub async fn add(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut spec): Json<AccountSpec>,
) -> Result<Json<Value>, AppError> {
    authorize_admin(&st.cfg, &headers)?;

    // validate before storing: a cookie without a usable token can never auth
    let probe = Account::new(spec.clone(), 0);
    if probe.token.is_none() {
        return Err(AppError::BadRequest(
            "missing token — copy localStorage userToken (JSON.parse(...).value) as well as the cookie".into(),
        ));
    }
    check_credential("token", probe.token.as_deref().unwrap_or_default())?;
    check_credential("cookie", &spec.cookie)?;

    if spec.email.trim().is_empty() {
        if let Ok(Some(resolved)) = tokio::time::timeout(
            std::time::Duration::from_millis(2500),
            st.ds.fetch_user_email(&probe),
        )
        .await
        {
            spec.email = resolved;
        }
    }

    let name = st.pool.add(spec);
    st.forget_session(&name).await;
    st.persist_accounts().await.map_err(AppError::Internal)?;
    Ok(Json(json!({ "ok": true, "name": name })))
}

pub async fn remove(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize_admin(&st.cfg, &headers)?;
    let removed = st.pool.remove(&name);
    st.forget_session(&name).await;
    st.persist_accounts().await.map_err(AppError::Internal)?;
    Ok(Json(json!({ "ok": removed, "name": name })))
}

pub async fn set_primary(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize_admin(&st.cfg, &headers)?;
    let updated = st.pool.set_primary(&name);
    if !updated {
        return Err(AppError::BadRequest(format!("account '{name}' not found")));
    }
    st.persist_accounts().await.map_err(AppError::Internal)?;
    Ok(Json(json!({ "ok": true, "primary": name, "accounts": st.pool.info() })))
}

/// Probes one specific named account without cross-key fallback, updating its
/// live error/cooldown status and resolving a missing email when possible.
pub async fn test_account(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize_admin(&st.cfg, &headers)?;
    let acc = st
        .pool
        .snapshot()
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| AppError::BadRequest(format!("account '{name}' not found")))?;

    if acc.email_str().is_empty() {
        if let Ok(Some(resolved)) = tokio::time::timeout(
            std::time::Duration::from_millis(1200),
            st.ds.fetch_user_email(&acc),
        )
        .await
        {
            if st.pool.set_email(&acc.name, &resolved) {
                let _ = st.persist_accounts().await;
            }
        }
    }

    let outcome = crate::api::openai::probe_account(&st, &acc).await;
    let acc_info = st
        .pool
        .info()
        .into_iter()
        .find(|v| v.get("name").and_then(|s| s.as_str()) == Some(name.as_str()));

    Ok(match outcome {
        Ok((reply, ms)) => Json(json!({
            "ok": true,
            "name": name,
            "latency_ms": ms as u64,
            "reply": reply,
            "account": acc_info,
        })),
        Err(err) => Json(json!({
            "ok": false,
            "name": name,
            "latency_ms": Value::Null,
            "error": err,
            "account": acc_info,
        })),
    })
}

/// Gracefully shuts down the local `api-ai` backend server when requested by the UI.
pub async fn shutdown(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    authorize_admin(&st.cfg, &headers)?;
    let tx = st.shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let _ = tx.send(true);
    });
    tracing::info!("graceful shutdown requested via /admin/shutdown");
    Ok((
        StatusCode::OK,
        [(header::CONNECTION, "close")],
        Json(json!({ "ok": true, "shutting_down": true })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_config(admin_key: Option<&str>) -> Arc<Config> {
        Arc::new(Config {
            host: "127.0.0.1".into(),
            port: 8787,
            api_keys: Vec::new(),
            admin_key: admin_key.map(str::to_string),
            upstream: "https://chat.deepseek.com".into(),
            user_agent: "ua".into(),
            app_version: "1".into(),
            client_version: "1".into(),
            completion_path: "/api/v0/chat/completion".into(),
            pow_enabled: false,
            reuse_session: false,
            delete_session: true,
            accounts_file: "accounts.test.json".into(),
            default_model: "deepseek-chat".into(),
        })
    }

    #[tokio::test]
    async fn shutdown_requires_valid_admin_key_and_signals_watch() {
        let st_locked = Arc::new(AppState::new(test_config(Some("secret-k"))).await.unwrap());
        let bad_hdrs = HeaderMap::new();
        assert!(shutdown(State(st_locked.clone()), bad_hdrs).await.is_err());

        let mut good_hdrs = HeaderMap::new();
        good_hdrs.insert("x-admin-key", "secret-k".parse().unwrap());
        let mut rx = st_locked.shutdown_tx.subscribe();
        assert!(shutdown(State(st_locked), good_hdrs).await.is_ok());
        tokio::time::timeout(std::time::Duration::from_millis(300), rx.changed())
            .await
            .expect("shutdown signal emitted")
            .unwrap();
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn list_backfills_missing_account_email_from_upstream() {
        use axum::{routing::get, Router};
        let app = Router::new().route(
            "/api/v0/users/current",
            get(|| async {
                Json(json!({
                    "code": 0,
                    "data": { "biz_data": { "email": "edge***@gmail.com" } }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let tmp_file = std::env::temp_dir().join(format!("acc_backfill_{}.json", std::process::id()));
        let mut cfg = (*test_config(None)).clone();
        cfg.upstream = format!("http://{addr}");
        cfg.accounts_file = tmp_file.to_string_lossy().to_string();
        let st = Arc::new(AppState::new(Arc::new(cfg)).await.unwrap());
        st.pool.add(AccountSpec {
            name: "edge:Default".into(),
            cookie: String::new(),
            token: Some("tok-edge-ok-123".into()),
            source: "edge".into(),
            profile: "Edge".into(),
            email: String::new(),
        });

        let Json(out) = list(State(st.clone()), HeaderMap::new()).await.unwrap();
        assert_eq!(out["accounts"][0]["email"], "edge***@gmail.com");
        let persisted = std::fs::read_to_string(&tmp_file).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp_file);
        assert!(persisted.contains("edge***@gmail.com"));
    }

    #[tokio::test]
    async fn test_account_isolates_selected_key_and_updates_cooldown() {
        use axum::{
            body::Body,
            http::header::CONTENT_TYPE,
            routing::post,
            Router,
        };
        let app = Router::new()
            .route(
                "/api/v0/chat_session/create",
                post(|headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth.contains("tok-edge-limit") {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({ "code": 429, "msg": "rate limit" })),
                        )
                            .into_response();
                    }
                    Json(json!({
                        "code": 0,
                        "data": { "biz_data": { "chat_session": { "id": "sess-test-1" } } }
                    }))
                    .into_response()
                }),
            )
            .route(
                "/api/v0/chat_session/delete",
                post(|| async { Json(json!({ "code": 0 })) }),
            )
            .route(
                "/api/v0/chat/completion",
                post(|| async {
                    let sse = "data: {\"v\":\"OK\"}\n\ndata: [DONE]\n\n";
                    axum::response::Response::builder()
                        .status(200)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(sse))
                        .unwrap()
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let tmp_file = std::env::temp_dir().join(format!("acc_diag_{}.json", std::process::id()));
        let mut cfg = (*test_config(None)).clone();
        cfg.upstream = format!("http://{addr}");
        cfg.accounts_file = tmp_file.to_string_lossy().to_string();
        let st = Arc::new(AppState::new(Arc::new(cfg)).await.unwrap());
        st.pool.add(AccountSpec {
            name: "chrome:Default".into(),
            cookie: String::new(),
            token: Some("tok-chrome-ok".into()),
            source: "chrome".into(),
            profile: "Chrome".into(),
            email: "chrome***@gmail.com".into(),
        });
        st.pool.add(AccountSpec {
            name: "edge:Default".into(),
            cookie: String::new(),
            token: Some("tok-edge-limit".into()),
            source: "edge".into(),
            profile: "Edge".into(),
            email: "edge***@gmail.com".into(),
        });

        // Testing edge:Default must NOT fall back to healthy chrome:Default.
        let Json(res_edge) = test_account(
            State(st.clone()),
            HeaderMap::new(),
            Path("edge:Default".into()),
        )
        .await
        .unwrap();
        assert_eq!(res_edge["ok"], false);
        assert!(res_edge["error"]
            .as_str()
            .unwrap_or("")
            .contains("429"));
        assert_eq!(res_edge["account"]["cooling_down"], true);

        // Testing chrome:Default succeeds cleanly on its own.
        let Json(res_chrome) = test_account(
            State(st.clone()),
            HeaderMap::new(),
            Path("chrome:Default".into()),
        )
        .await
        .unwrap();
        assert_eq!(res_chrome["ok"], true);
        assert_eq!(res_chrome["reply"], "OK");
        assert!(res_chrome["latency_ms"].as_u64().is_some());

        // Unknown account returns BadRequest.
        let missing = test_account(
            State(st),
            HeaderMap::new(),
            Path("unknown:Slot".into()),
        )
        .await;
        assert!(missing.is_err());
        let _ = std::fs::remove_file(&tmp_file);
    }
}
