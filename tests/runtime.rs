use axum::body::Body;
use axum::http::{Request, StatusCode};
use insist::config::Config;
use insist::runtime::{Clock, Runtime};
use insist::secrets::Secrets;
use insist::server::router;
use insist::state::{Loaded, State};
use jiff::{SignedDuration, Timestamp};
use std::sync::{Arc, Mutex as StdMutex};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn recorded(name: &str) -> String {
    std::fs::read_to_string(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

struct World {
    ntfy: MockServer,
    am: MockServer,
    dog: MockServer,
    dir: tempfile::TempDir,
    now: Arc<StdMutex<Timestamp>>,
    shared: insist::server::Shared,
}

impl World {
    async fn new(ntfy_code: u16) -> World {
        let ntfy = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(ntfy_code))
            .mount(&ntfy)
            .await;
        let am = MockServer::start().await;
        let dog = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ping/x"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&dog)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let minimal = include_str!("../fixtures/constructed-config.toml");
        let toml = minimal
            .replace("https://ntfy.example", &ntfy.uri())
            .replace("http://127.0.0.1:9093", &am.uri())
            .replace(
                "/var/lib/insist/state.json",
                dir.path().join("state.json").to_str().unwrap(),
            );
        let config = Config::from_toml(&toml).unwrap();
        let secrets = Secrets::parse(&format!(
            "NTFY_TOPIC=alarmtopic\nNTFY_TOKEN=tk_alarm\nNTFY_BUTTON_TOKEN=tk_knopf\nACK_HMAC_KEY=3f9a0c1e5b7d2f4a\nWATCHDOG_URL={}/ping/x\n",
            dog.uri()
        ))
        .unwrap();
        let webhook: insist::alertmanager::WebhookMessage =
            serde_json::from_str(&recorded("alertmanager-0.31.1/webhook-firing-single.json"))
                .unwrap();
        let now = Arc::new(StdMutex::new(webhook.alerts[0].starts_at));
        let clock_now = now.clone();
        let clock: Clock = Arc::new(move || *clock_now.lock().unwrap());
        let loaded = Loaded {
            state: State::default(),
            moved_corrupt_to: None,
        };
        let runtime = Runtime::new(config, secrets, loaded, clock).unwrap();
        World {
            ntfy,
            am,
            dog,
            dir,
            now,
            shared: Arc::new(tokio::sync::Mutex::new(runtime)),
        }
    }

    fn advance(&self, secs: i64) {
        let mut n = self.now.lock().unwrap();
        *n += SignedDuration::from_secs(secs);
    }

    async fn call(&self, req: Request<Body>) -> (StatusCode, String) {
        let res = router(self.shared.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn post_webhook(&self, name: &str) -> StatusCode {
        self.call(
            Request::post("/")
                .header("content-type", "application/json")
                .body(Body::from(recorded(name)))
                .unwrap(),
        )
        .await
        .0
    }

    async fn published(&self) -> Vec<serde_json::Value> {
        self.ntfy
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }
}

#[tokio::test]
async fn a_webhook_becomes_a_notification_with_sequence_id_and_button_and_is_saved() {
    let w = World::new(200).await;
    assert_eq!(
        w.post_webhook("alertmanager-0.31.1/webhook-firing-single.json")
            .await,
        StatusCode::OK
    );
    let p = w.published().await;
    assert_eq!(p.len(), 1);
    assert_eq!(p[0]["topic"], "alarmtopic");
    assert_eq!(p[0]["priority"], 4);
    let seq = p[0]["sequence_id"].as_str().unwrap();
    assert_eq!(seq.len(), 16);
    let action = &p[0]["actions"][0];
    assert!(action["url"]
        .as_str()
        .unwrap()
        .ends_with("/alarmtopic-quittung"));
    assert_eq!(action["headers"]["Authorization"], "Bearer tk_knopf");
    assert!(action["body"]
        .as_str()
        .unwrap()
        .starts_with(&format!("a1.{seq}.")));
    let saved = std::fs::read_to_string(w.dir.path().join("state.json")).unwrap();
    assert!(saved.contains(seq));
}

#[tokio::test]
async fn a_failed_reconcile_keeps_everything_and_starves_the_dead_mans_switch() {
    let w = World::new(200).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/alerts"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&w.am)
        .await;
    w.post_webhook("alertmanager-0.31.1/webhook-firing-single.json")
        .await;
    assert!(!w.shared.lock().await.reconcile_once().await);
    assert_eq!(w.shared.lock().await.engine.open_instances(), 1);
    let (code, _) = w
        .call(Request::post("/watchdog").body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert!(w.dog.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn after_a_good_reconcile_the_watchdog_ping_is_forwarded_until_it_goes_stale() {
    let w = World::new(200).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/alerts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(recorded("alertmanager-0.31.1/api-empty.json")),
        )
        .mount(&w.am)
        .await;
    assert!(w.shared.lock().await.reconcile_once().await);
    let (code, _) = w
        .call(
            Request::post("/watchdog")
                .body(Body::from("{\"x\":1}"))
                .unwrap(),
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(w.dog.received_requests().await.unwrap().len(), 1);
    w.advance(301);
    let (code, _) = w
        .call(Request::post("/watchdog").body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn ntfy_refusing_shows_up_as_pending_in_the_metrics() {
    let w = World::new(500).await;
    assert_eq!(
        w.post_webhook("alertmanager-0.31.1/webhook-firing-single.json")
            .await,
        StatusCode::OK
    );
    let (_, metrics) = w
        .call(Request::get("/metrics").body(Body::empty()).unwrap())
        .await;
    assert!(metrics.contains("insist_publish_pending 1"), "{metrics}");
    assert!(metrics.contains("insist_open_instances 1"), "{metrics}");
}

#[tokio::test]
async fn a_button_press_replaces_the_notification_and_a_forged_one_is_counted() {
    let w = World::new(200).await;
    w.post_webhook("alertmanager-0.31.1/webhook-firing-single.json")
        .await;
    let first = w.published().await.remove(0);
    let body = first["actions"][0]["body"].as_str().unwrap().to_string();

    let forged = insist::ntfy::StreamLine {
        id: "m1".into(),
        event: "message".into(),
        message: Some("a1.0123456789abcdef.00000000000000000000000000000000".into()),
    };
    w.shared.lock().await.acknowledge(forged).await;
    let pressed = insist::ntfy::StreamLine {
        id: "m2".into(),
        event: "message".into(),
        message: Some(body),
    };
    w.shared.lock().await.acknowledge(pressed).await;

    let p = w.published().await;
    let last = p.last().unwrap();
    assert_eq!(last["sequence_id"], first["sequence_id"]);
    assert!(last["title"].as_str().unwrap().starts_with("Acknowledged"));
    assert!(last.get("actions").is_none());
    let (_, metrics) = w
        .call(Request::get("/metrics").body(Body::empty()).unwrap())
        .await;
    assert!(metrics.contains("insist_ack_rejected_total 1"), "{metrics}");
    assert_eq!(
        w.shared.lock().await.engine.state().ack_cursor.as_deref(),
        Some("m2")
    );
}

#[tokio::test]
async fn after_an_hour_unacknowledged_the_mail_alert_is_raised_in_alertmanager() {
    let w = World::new(200).await;
    Mock::given(method("POST"))
        .and(path("/api/v2/alerts"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&w.am)
        .await;
    w.post_webhook("alertmanager-0.31.1/webhook-firing-single.json")
        .await;
    for secs in [900, 2700] {
        w.advance(secs);
        w.shared.lock().await.tick_once().await;
    }
    let raised: Vec<serde_json::Value> =
        w.am.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
    assert_eq!(raised.len(), 1);
    let labels = &raised[0][0]["labels"];
    assert_eq!(labels["alertname"], "AlarmUnquittiert");
    assert_eq!(labels["insist"], "unacknowledged");
    assert_eq!(labels["severity"], "critical");
    assert!(raised[0][0]["annotations"]["summary"]
        .as_str()
        .unwrap()
        .contains("UnitFehlgeschlagen"));
}

#[tokio::test]
async fn an_unreadable_webhook_is_a_500() {
    let w = World::new(200).await;
    let (code, _) = w
        .call(Request::post("/").body(Body::from("{nope")).unwrap())
        .await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
}
