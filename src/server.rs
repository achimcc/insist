//! HTTP side of stage 1.
use crate::alertmanager::WebhookMessage;
use crate::metrics::MetricsHandle;
use crate::runtime::{send_watchdog, Runtime};
use axum::body::Bytes;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub type Shared = Arc<tokio::sync::Mutex<Runtime>>;

/// Two independent pieces of state, on purpose: `/metrics` must answer
/// without ever taking the runtime's own lock (see `runtime::Runtime`'s
/// module doc), so it gets its own `FromRef` extraction straight to the
/// `MetricsHandle` rather than going through `Shared`.
#[derive(Clone)]
pub struct App {
    pub runtime: Shared,
    pub metrics: MetricsHandle,
}

impl FromRef<App> for Shared {
    fn from_ref(app: &App) -> Shared {
        app.runtime.clone()
    }
}

impl FromRef<App> for MetricsHandle {
    fn from_ref(app: &App) -> MetricsHandle {
        app.metrics.clone()
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", post(webhook))
        .route("/watchdog", post(watchdog))
        .route("/health", get(|| async { "insist" }))
        .route("/metrics", get(metrics))
        .with_state(app)
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
    // The lock covers only the freshness check; the POST itself (up to 10 s)
    // runs after the guard from `.lock().await` has already been dropped.
    let target = shared.lock().await.watchdog_target();
    match target {
        Err(code) => code,
        Ok((client, url)) => send_watchdog(client, url, body).await,
    }
}

async fn metrics(State(handle): State<MetricsHandle>) -> String {
    handle.lock().unwrap().render()
}
