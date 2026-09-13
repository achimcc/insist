//! HTTP side of stage 1.
use crate::alertmanager::WebhookMessage;
use crate::runtime::Runtime;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub type Shared = Arc<tokio::sync::Mutex<Runtime>>;

pub fn router(shared: Shared) -> Router {
    Router::new()
        .route("/", post(webhook))
        .route("/watchdog", post(watchdog))
        .route("/health", get(|| async { "insist" }))
        .route("/metrics", get(metrics))
        .with_state(shared)
}

async fn webhook(State(shared): State<Shared>, body: Bytes) -> (StatusCode, &'static str) {
    // 5xx, not 4xx: Alertmanager retries a 5xx; a 4xx drops the alert.
    let message: WebhookMessage = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("unreadable webhook body: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "unreadable body");
        }
    };
    // Stateful instances are accepted once they are in the state: a failed
    // send is retried by the tick. Only stateless sends fail the webhook.
    if shared.lock().await.webhook(&message).await == 0 {
        (StatusCode::OK, "ok")
    } else {
        (
            StatusCode::BAD_GATEWAY,
            "ntfy did not accept every notification",
        )
    }
}

async fn watchdog(State(shared): State<Shared>, body: Bytes) -> StatusCode {
    shared.lock().await.forward_watchdog(body).await
}

async fn metrics(State(shared): State<Shared>) -> String {
    shared.lock().await.metrics.render()
}
