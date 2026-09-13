//! HTTP side. Stage 0: a stateless translator from Alertmanager's webhook to
//! ntfy, replacing the homeserver's alarm-uebersetzer one for one.
use crate::alertmanager::{Labels, WebhookMessage};
use crate::config::{Config, ProbeMatch};
use crate::ntfy::{NtfyClient, Publication};
use crate::render::render_alert;
use crate::secret::Secret;
use crate::secrets::Secrets;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub struct Stage0 {
    pub config: Config,
    pub secrets: Secrets,
    pub ntfy: NtfyClient,
    pub tz: jiff::tz::TimeZone,
}

pub fn topic_for(labels: &Labels, probe: &ProbeMatch, topic: &Secret) -> Secret {
    if labels.get(&probe.label) == Some(&probe.value) {
        Secret::from(format!("{}{}", topic.expose(), probe.topic_suffix))
    } else {
        Secret::from(topic.expose().to_string())
    }
}

pub fn router_stage0(app: Arc<Stage0>) -> Router {
    Router::new()
        .route("/", post(webhook_stage0))
        .route("/health", get(|| async { "insist" }))
        .with_state(app)
}

async fn webhook_stage0(State(app): State<Arc<Stage0>>, body: Bytes) -> (StatusCode, &'static str) {
    // 5xx, not 4xx: Alertmanager retries a 5xx, and a body we cannot read
    // today may be one a fixed version reads tomorrow. A 4xx drops the alert.
    let message: WebhookMessage = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("unreadable webhook body: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "unreadable body");
        }
    };
    let mut failed = false;
    for alert in &message.alerts {
        let r = render_alert(
            &alert.labels,
            &alert.annotations,
            alert.starts_at,
            alert.status == "resolved",
            &app.config.texts,
            &app.tz,
        );
        let publication = Publication {
            topic: topic_for(&alert.labels, &app.config.probe, &app.secrets.topic),
            title: r.title.clone(),
            message: r.message,
            priority: r.priority,
            tags: r.tags,
            sequence_id: None,
            actions: vec![],
        };
        match app.ntfy.publish(&publication).await {
            Ok(()) => tracing::info!("delivered: {}", r.title),
            Err(e) => {
                tracing::error!("not delivered: {}: {e}", r.title);
                failed = true;
            }
        }
    }
    if failed {
        (
            StatusCode::BAD_GATEWAY,
            "ntfy did not accept every notification",
        )
    } else {
        (StatusCode::OK, "ok")
    }
}
