use axum::body::Body;
use axum::http::{Request, StatusCode};
use insist::config::Config;
use insist::ntfy::NtfyClient;
use insist::secret::Secret;
use insist::secrets::Secrets;
use insist::server::{router_stage0, Stage0};
use std::sync::Arc;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn recorded(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/fixtures/alertmanager-0.31.1/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn app(ntfy: &MockServer) -> axum::Router {
    let config = Config::from_toml(&format!(
        r#"
listen = "127.0.0.1:9099"
ntfy_url = "{}"
alertmanager_url = "http://127.0.0.1:9093"
secrets_file = "/run/credentials/insist.service/insist-env"
state_file = "/var/lib/insist/state.json"
timezone = "Europe/Berlin"
ack_topic_suffix = "-quittung"
reconcile_secs = 60
tick_secs = 15
watchdog_max_age_secs = 300
unacknowledged_alertname = "AlarmUnquittiert"
receivers = ["rec"]

[probe]
label = "insist_probe"
value = "ja"
topic_suffix = "-selbstprobe"

[night]
start_hour = 22
end_hour = 7

[ladders.critical]
steps = [
  {{ after_secs = 0, priority = 4 }},
  {{ after_secs = 900, repeat_secs = 900, priority = 5 }},
  {{ after_secs = 3600, repeat_secs = 300, priority = 5, raise_unacknowledged = true }},
]

[ladders.warning]
quiet_at_night = true
steps = [
  {{ after_secs = 0, priority = 3 }},
  {{ after_secs = 14400, repeat_secs = 43200, priority = 4 }},
]

[ladders.probe]
steps = [
  {{ after_secs = 0, priority = 4 }},
  {{ after_secs = 60, repeat_secs = 60, priority = 5 }},
]
"#,
        ntfy.uri()
    ))
    .unwrap();
    let secrets = Secrets::parse("NTFY_TOPIC=alarmtopic\nNTFY_TOKEN=tk_alarm\nNTFY_BUTTON_TOKEN=tk_knopf\nACK_HMAC_KEY=00ff\nWATCHDOG_URL=https://hc.example/ping/geheim\n").unwrap();
    let tz = config.tz().unwrap();
    let client = NtfyClient::new(&config.ntfy_url, Secret::from("tk_alarm".to_string())).unwrap();
    router_stage0(Arc::new(Stage0 {
        config,
        secrets,
        ntfy: client,
        tz,
    }))
}

async fn post(app: axum::Router, body: String) -> StatusCode {
    app.oneshot(
        Request::post("/")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

fn published_bodies(requests: &[wiremock::Request]) -> Vec<serde_json::Value> {
    requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn a_recorded_group_becomes_one_notification_per_alert() {
    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&ntfy)
        .await;
    assert_eq!(
        post(app(&ntfy).await, recorded("webhook-firing-group.json")).await,
        StatusCode::OK
    );
    let bodies = published_bodies(&ntfy.received_requests().await.unwrap());
    assert_eq!(bodies.len(), 2);
    assert!(bodies
        .iter()
        .all(|b| b["topic"] == "alarmtopic" && b["title"] == "GastNichtAktiv"));
}

#[tokio::test]
async fn ntfy_refusing_is_a_502_so_alertmanager_retries() {
    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&ntfy)
        .await;
    assert_eq!(
        post(app(&ntfy).await, recorded("webhook-firing-single.json")).await,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn an_unreadable_body_is_a_5xx_not_a_4xx() {
    let ntfy = MockServer::start().await;
    assert_eq!(
        post(app(&ntfy).await, "{not json".into()).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn a_probe_goes_to_the_probe_topic() {
    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&ntfy)
        .await;
    // constructed from the recorded single alert by adding one label
    let mut v: serde_json::Value =
        serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
    v["alerts"][0]["labels"]["insist_probe"] = "ja".into();
    assert_eq!(post(app(&ntfy).await, v.to_string()).await, StatusCode::OK);
    let bodies = published_bodies(&ntfy.received_requests().await.unwrap());
    assert_eq!(bodies[0]["topic"], "alarmtopic-selbstprobe");
}

#[tokio::test]
async fn health_answers() {
    let ntfy = MockServer::start().await;
    let res = app(&ntfy)
        .await
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
