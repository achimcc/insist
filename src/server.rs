//! HTTP side of stage 1.
use crate::alertmanager::WebhookMessage;
use crate::metrics::MetricsHandle;
use crate::runtime::{send_watchdog, Runtime};
use crate::secret::Secret;
use axum::body::Bytes;
use axum::extract::{FromRef, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub type Shared = Arc<tokio::sync::Mutex<Runtime>>;

/// The bearer token the two writing endpoints demand, lifted out of
/// `Secrets` at startup. It sits in `App` rather than behind the runtime
/// lock on purpose: the guard runs before every such request, and taking
/// the lock there would serialise the check against the work it guards.
#[derive(Clone)]
pub struct WebhookToken(pub Secret);

/// Three independent pieces of state, on purpose: `/metrics` must answer
/// without ever taking the runtime's own lock (see `runtime::Runtime`'s
/// module doc), so it gets its own `FromRef` extraction straight to the
/// `MetricsHandle` rather than going through `Shared`.
#[derive(Clone)]
pub struct App {
    pub runtime: Shared,
    pub metrics: MetricsHandle,
    pub webhook_token: WebhookToken,
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

impl FromRef<App> for WebhookToken {
    fn from_ref(app: &App) -> WebhookToken {
        app.webhook_token.clone()
    }
}

/// WHY THE SPLIT IS STRUCTURAL AND NOT A LINE IN EACH HANDLER: the two
/// writing endpoints change state — a forged `resolved` deletes an instance
/// and holds the escalation at stage 0, a forged ping tells the dead man's
/// switch this machine is alive (audit B41). The two reading ones must stay
/// open: Prometheus scrapes `/metrics` and systemd probes `/health`, neither
/// carries a credential. Written as two routers, a new endpoint has to be
/// put on one side or the other, and the choice is visible in the diff. A
/// check inside each handler can be forgotten by the next one.
pub fn router(app: App) -> Router {
    let guarded = Router::new()
        .route("/", post(webhook))
        .route("/watchdog", post(watchdog))
        .route_layer(middleware::from_fn_with_state(
            app.webhook_token.clone(),
            require_token,
        ));
    let open = Router::new()
        .route("/health", get(|| async { "insist" }))
        .route("/metrics", get(metrics));
    guarded.merge(open).with_state(app)
}

/// 401 and never 5xx: Alertmanager retries a 5xx, and a retry cannot fix a
/// wrong token — it would only turn one rejected delivery into an endless
/// stream of them. The body names nothing; whether the header was missing
/// or merely wrong is not the caller's business.
async fn require_token(
    State(WebhookToken(expected)): State<WebhookToken>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, &'static str)> {
    let offered = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match offered {
        // `Secret::matches` compares in constant time: a plain `==` returns
        // at the first differing byte and leaks the prefix to anyone who can
        // time the answer.
        Some(token) if expected.matches(token) => Ok(next.run(request).await),
        _ => {
            // The value never reaches the log, only the fact of the refusal.
            tracing::warn!("refused a request without a valid bearer token");
            Err((StatusCode::UNAUTHORIZED, "unauthorized"))
        }
    }
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
        Ok(target) => send_watchdog(target, body).await,
    }
}

async fn metrics(State(handle): State<MetricsHandle>) -> String {
    handle.lock().unwrap().render()
}
