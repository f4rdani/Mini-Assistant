mod api;
pub mod config;
mod convert;
mod deepseek;
mod error;
mod state;
mod util;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use axum::Router;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower_http::cors::{Any, CorsLayer};

/// Running in-process HTTP server handle (used by the portable single-binary release).
pub struct EmbeddedServer {
    pub addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl EmbeddedServer {
    /// Signals graceful shutdown of the embedded Axum server.
    pub fn shutdown(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

fn build_router(state: Arc<state::AppState>) -> Router {
    let app = Router::new()
        .route("/health", get(api::health))
        .route("/v1/models", get(api::openai::models))
        .route(
            "/v1/chat/completions",
            // Vision sends base64 images inside the JSON body; 4 MB is too
            // tight for a 4K screenshot once it is base64-encoded.
            post(api::openai::chat_completions).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/v1/files",
            post(api::openai::upload_file).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/admin/accounts",
            get(api::admin::list).post(api::admin::add),
        )
        .route("/admin/accounts/{name}", delete(api::admin::remove))
        .route(
            "/admin/accounts/{name}/primary",
            post(api::admin::set_primary),
        )
        .route(
            "/admin/accounts/{name}/test",
            post(api::admin::test_account),
        )
        .route("/admin/shutdown", post(api::admin::shutdown))
        // everything else stays small
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    #[cfg(feature = "standalone")]
    let app = app.layer(tower_http::trace::TraceLayer::new_for_http());

    app.with_state(state)
}

async fn await_shutdown(mut shutdown_rx: watch::Receiver<bool>) {
    while !*shutdown_rx.borrow_and_update() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }
}

/// Starts the API server in-process on the caller's Tokio runtime.
///
/// Binds the TCP listener before returning so callers can immediately issue
/// requests with zero startup sleep or race conditions.
pub async fn start_embedded(cfg: config::Config) -> anyhow::Result<EmbeddedServer> {
    let cfg = Arc::new(cfg);
    let state = Arc::new(state::AppState::new(cfg.clone()).await?);
    {
        let st_bg = state.clone();
        tokio::spawn(async move {
            api::admin::backfill_missing_emails(&st_bg).await;
        });
    }

    let shutdown_tx = state.shutdown_tx.clone();
    let shutdown_rx = state.shutdown_tx.subscribe();
    let app = build_router(state);

    let bind_addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    let addr = listener.local_addr()?;

    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(await_shutdown(shutdown_rx))
            .await;
    });

    Ok(EmbeddedServer {
        addr,
        shutdown_tx,
        task: Some(task),
    })
}

/// Runs the full standalone development/server daemon with `.env` & tracing logs.
#[cfg(feature = "standalone")]
pub async fn run_standalone() -> anyhow::Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,api_ai=debug,tower_http=warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = Arc::new(config::Config::from_env()?);
    let state = Arc::new(state::AppState::new(cfg.clone()).await?);
    {
        let st_bg = state.clone();
        tokio::spawn(async move {
            api::admin::backfill_missing_emails(&st_bg).await;
        });
    }

    let shutdown_rx = state.shutdown_tx.subscribe();
    let app = build_router(state);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("api-ai listening on http://{addr}");
    if !cfg.is_loopback() && cfg.admin_key.is_none() {
        tracing::warn!("bound to {addr} without ADMIN_KEY — /admin/* is disabled");
    }
    if !cfg.auth_required() {
        tracing::warn!(
            "API_KEYS is empty: anything that can reach {addr} can spend your DeepSeek quota (browser pages included). Set API_KEYS before exposing this."
        );
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(await_shutdown(shutdown_rx))
        .await?;
    Ok(())
}

#[cfg(test)]
mod embedded_tests {
    use super::*;

    #[tokio::test]
    async fn start_embedded_binds_serves_health_and_shuts_down() {
        let mut cfg = config::Config::from_env().unwrap();
        cfg.host = "127.0.0.1".into();
        cfg.port = 0; // ephemeral port
        cfg.accounts_file = std::env::temp_dir()
            .join(format!("api_ai_embed_test_{}.json", std::process::id()))
            .to_string_lossy()
            .to_string();

        let mut srv = start_embedded(cfg).await.expect("start embedded api");
        let url = format!("http://{}/health", srv.addr);
        let resp = reqwest::get(&url).await.expect("health request");
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.expect("json");
        assert_eq!(body["status"], "ok");

        srv.shutdown();
    }
}
