use axum::body::Body;
use axum::http::{Request, StatusCode};
use insist::config::Config;
use insist::metrics::MetricsHandle;
use insist::runtime::{Clock, Runtime};
use insist::secrets::Secrets;
use insist::server::{router, App, Shared};
use insist::state::{Loaded, State};
use jiff::{SignedDuration, Timestamp};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
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
    runtime: Shared,
    metrics: MetricsHandle,
}

impl World {
    async fn new(ntfy_code: u16) -> World {
        World::with_watchdog_url(ntfy_code, None).await
    }

    /// `watchdog_url`: where the dead man's switch is expected; `None` is
    /// the `dog` mock, which answers 200.
    async fn with_watchdog_url(ntfy_code: u16, watchdog_url: Option<String>) -> World {
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
            "NTFY_TOPIC=alarmtopic\nNTFY_TOKEN=tk_alarm\nNTFY_BUTTON_TOKEN=tk_knopf\nACK_HMAC_KEY=3f9a0c1e5b7d2f4a\nWATCHDOG_URL={}\n",
            watchdog_url.unwrap_or_else(|| format!("{}/ping/x", dog.uri()))
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
        let metrics = runtime.metrics_handle();
        World {
            ntfy,
            am,
            dog,
            dir,
            now,
            runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
            metrics,
        }
    }

    fn app(&self) -> App {
        App {
            runtime: self.runtime.clone(),
            metrics: self.metrics.clone(),
        }
    }

    fn advance(&self, secs: i64) {
        let mut n = self.now.lock().unwrap();
        *n += SignedDuration::from_secs(secs);
    }

    async fn call(&self, req: Request<Body>) -> (StatusCode, String) {
        let res = router(self.app()).oneshot(req).await.unwrap();
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
    assert!(!w.runtime.lock().await.reconcile_once().await);
    assert_eq!(w.runtime.lock().await.engine.open_instances(), 1);
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
    assert!(w.runtime.lock().await.reconcile_once().await);
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
    w.runtime.lock().await.acknowledge(forged).await;
    let pressed = insist::ntfy::StreamLine {
        id: "m2".into(),
        event: "message".into(),
        message: Some(body),
    };
    w.runtime.lock().await.acknowledge(pressed).await;

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
        w.runtime.lock().await.engine.state().ack_cursor.as_deref(),
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
        w.runtime.lock().await.tick_once().await;
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
    // Sent explicitly: Alertmanager 0.31.1 fills an omitted startsAt with
    // endsAt (measured 2026-09-14), which would date the mail an hour ahead.
    let now = *w.now.lock().unwrap();
    assert_eq!(raised[0][0]["startsAt"], now.to_string());
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

// --- Fix round 1: bounded pass, raise before publish, lock-free metrics and
// watchdog, ack cursor on disk. ---

/// ntfy hanging (not refusing) is the realistic failure this bounds: with two
/// due instances and a delay past the client's send timeout, a pass must
/// call ntfy at most once and count both as pending rather than blocking the
/// runtime lock for `N * timeout`.
#[tokio::test]
async fn a_hanging_ntfy_bounds_the_pass_to_one_attempt_and_marks_the_rest_pending() {
    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(12)))
        .mount(&ntfy)
        .await;
    let am = MockServer::start().await;
    let dog = MockServer::start().await;
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
    let group: insist::alertmanager::WebhookMessage =
        serde_json::from_str(&recorded("alertmanager-0.31.1/webhook-firing-group.json")).unwrap();
    assert_eq!(group.alerts.len(), 2, "the fixture must carry two alerts");
    let now = Arc::new(StdMutex::new(group.alerts[0].starts_at));
    let clock_now = now.clone();
    let clock: Clock = Arc::new(move || *clock_now.lock().unwrap());
    let loaded = Loaded {
        state: State::default(),
        moved_corrupt_to: None,
    };
    let runtime = Runtime::new(config, secrets, loaded, clock).unwrap();
    let metrics = runtime.metrics_handle();
    let shared: Shared = Arc::new(tokio::sync::Mutex::new(runtime));

    let started = std::time::Instant::now();
    shared.lock().await.webhook(&group).await;
    // The client's own send timeout (10 s) bounds this, not two of them.
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}, a second send must have been skipped",
        started.elapsed()
    );
    assert_eq!(
        ntfy.received_requests().await.unwrap().len(),
        1,
        "only the first due send is attempted; the rest stay pending for the next pass"
    );
    let rendered = metrics.lock().unwrap().render();
    assert!(rendered.contains("insist_publish_pending 2"), "{rendered}");
}

#[tokio::test]
async fn a_raise_still_reaches_alertmanager_even_when_every_send_to_ntfy_is_refused() {
    let w = World::new(500).await;
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
        w.runtime.lock().await.tick_once().await;
    }
    let raised: Vec<serde_json::Value> =
        w.am.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
    assert_eq!(
        raised.len(),
        1,
        "the unacknowledged mail must reach Alertmanager even though every publish to ntfy failed"
    );
    assert_eq!(raised[0][0]["labels"]["alertname"], "AlarmUnquittiert");
}

#[tokio::test]
async fn metrics_answers_even_while_the_runtime_lock_is_held() {
    let w = World::new(200).await;
    let guard = w.runtime.lock().await;
    let fut = router(w.app()).oneshot(Request::get("/metrics").body(Body::empty()).unwrap());
    let res = tokio::time::timeout(Duration::from_millis(500), fut)
        .await
        .expect("a scrape must not wait behind the runtime lock")
        .unwrap();
    drop(guard);
    assert_eq!(res.status(), StatusCode::OK);
}

/// A storm must not be multiplied by the number of webhooks that carry it.
///
/// Audit finding B42, measured on 2026-09-14 against a fake ntfy answering
/// 429: `insist_publish_failures_total` reached **128100 in 100 seconds**
/// from around 500 alerts. Every webhook ends with a `tick`, a `tick` offers
/// every open instance that is due, and a failed publish leaves `last_sent`
/// alone — so every instance stayed due and was retried by every later
/// webhook. N alerts arriving as N webhooks cost N² requests, and they cost
/// them at exactly the moment ntfy is already saying it has had enough.
///
/// `Transport` already stopped a pass (an unreachable ntfy would otherwise
/// hold the runtime lock for `N * 10s`). A 429 is the same statement made
/// quickly: the answer is about ntfy, not about this one message, so the rest
/// of the pass will get it too.
#[tokio::test]
async fn a_refused_pass_stops_instead_of_asking_for_every_open_instance() {
    let w = World::new(429).await;
    let storm = a_storm_of(20);

    for _ in 0..2 {
        w.call(
            Request::post("/")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&storm).unwrap()))
                .unwrap(),
        )
        .await;
    }

    let attempts = w.ntfy.received_requests().await.unwrap().len();
    assert!(
        attempts <= 2,
        "{attempts} requests for two webhooks: ntfy said 429 and was asked again anyway"
    );

    // And nothing was quietly dropped: every instance is still owed.
    let (_, metrics) = w
        .call(Request::get("/metrics").body(Body::empty()).unwrap())
        .await;
    assert!(metrics.contains("insist_publish_pending 20"), "{metrics}");
    assert!(metrics.contains("insist_open_instances 20"), "{metrics}");
}

/// A 4xx that is about THIS message must not stop the pass — otherwise one
/// malformed notification holds up every other alert in the house. Only the
/// answers that are about ntfy itself do (429 and 5xx).
#[tokio::test]
async fn a_refusal_of_one_message_does_not_stop_the_others() {
    let w = World::new(400).await;
    let storm = a_storm_of(5);

    w.call(
        Request::post("/")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&storm).unwrap()))
            .unwrap(),
    )
    .await;

    assert_eq!(
        w.ntfy.received_requests().await.unwrap().len(),
        5,
        "a 400 is about the one message, so the rest must still be offered"
    );
}

/// `count` firing alerts in one webhook, each its own instance.
///
/// The SHAPE is the recorded answer; only the fingerprint and the instance
/// label vary, because an `InstanceId` is a fingerprint plus a `startsAt`.
fn a_storm_of(count: usize) -> serde_json::Value {
    let mut v: serde_json::Value =
        serde_json::from_str(&recorded("alertmanager-0.31.1/webhook-firing-single.json")).unwrap();
    let one = v["alerts"][0].clone();
    let alerts: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let mut a = one.clone();
            a["fingerprint"] = format!("{:016x}", 0x51_0000_0000_u64 + i as u64).into();
            a["labels"]["instance"] = format!("host{i}").into();
            a
        })
        .collect();
    v["alerts"] = alerts.into();
    v
}

#[tokio::test]
async fn a_once_alert_with_ntfy_failing_fails_the_webhook() {
    let w = World::new(500).await;
    let mut v: serde_json::Value =
        serde_json::from_str(&recorded("alertmanager-0.31.1/webhook-firing-single.json")).unwrap();
    v["alerts"][0]["labels"]["severity"] = "info".into();
    let (code, _) = w
        .call(
            Request::post("/")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&v).unwrap()))
                .unwrap(),
        )
        .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn acknowledge_saves_the_ack_cursor_to_disk() {
    let w = World::new(200).await;
    w.post_webhook("alertmanager-0.31.1/webhook-firing-single.json")
        .await;
    let first = w.published().await.remove(0);
    let body = first["actions"][0]["body"].as_str().unwrap().to_string();
    let line = insist::ntfy::StreamLine {
        id: "m9".into(),
        event: "message".into(),
        message: Some(body),
    };
    w.runtime.lock().await.acknowledge(line).await;
    let saved = std::fs::read_to_string(w.dir.path().join("state.json")).unwrap();
    assert!(saved.contains("m9"), "{saved}");
}

#[tokio::test]
async fn a_future_start_is_counted_once_on_the_metrics() {
    let w = World::new(200).await;
    let (_, metrics) = w
        .call(Request::get("/metrics").body(Body::empty()).unwrap())
        .await;
    assert!(
        metrics.contains("\ninsist_future_starts_total 0\n"),
        "{metrics}"
    );

    // Constructed from the recorded webhook: startsAt a day ahead of the
    // clock, the shape Alertmanager gives an alert posted with endsAt only.
    let mut v: serde_json::Value =
        serde_json::from_str(&recorded("alertmanager-0.31.1/webhook-firing-single.json")).unwrap();
    let ahead = *w.now.lock().unwrap() + SignedDuration::from_hours(26);
    v["alerts"][0]["startsAt"] = ahead.to_string().into();
    let body = v.to_string();
    for _ in 0..2 {
        let (code, _) = w
            .call(
                Request::post("/")
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await;
        assert_eq!(code, StatusCode::OK);
        w.advance(30);
        w.runtime.lock().await.tick_once().await;
    }
    let (_, metrics) = w
        .call(Request::get("/metrics").body(Body::empty()).unwrap())
        .await;
    assert!(
        metrics.contains("\ninsist_future_starts_total 1\n"),
        "{metrics}"
    );
}

/// The value of one metric line, read by name from `/metrics`.
fn metric(text: &str, name: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| panic!("{name} missing from:\n{text}"))
        .to_string()
}

async fn good_reconcile(w: &World) {
    Mock::given(method("GET"))
        .and(path("/api/v2/alerts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(recorded("alertmanager-0.31.1/api-empty.json")),
        )
        .mount(&w.am)
        .await;
    assert!(w.runtime.lock().await.reconcile_once().await);
}

async fn scrape(w: &World) -> String {
    w.call(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .1
}

#[tokio::test]
async fn a_forwarded_watchdog_ping_sets_the_success_gauge_and_counts_no_failure() {
    let w = World::new(200).await;
    good_reconcile(&w).await;
    let before = scrape(&w).await;
    assert_eq!(
        metric(&before, "insist_watchdog_last_success_timestamp_seconds"),
        "0"
    );
    let (code, _) = w
        .call(Request::post("/watchdog").body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(w.dog.received_requests().await.unwrap().len(), 1);
    let after = scrape(&w).await;
    let now = w.now.lock().unwrap().as_second().to_string();
    assert_eq!(
        metric(&after, "insist_watchdog_last_success_timestamp_seconds"),
        now
    );
    assert_eq!(
        metric(&after, "insist_watchdog_forward_failures_total"),
        "0"
    );
}

#[tokio::test]
async fn a_refused_watchdog_forward_is_counted_and_leaves_the_success_gauge_alone() {
    let w = World::new(200).await;
    good_reconcile(&w).await;
    let (code, _) = w
        .call(Request::post("/watchdog").body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(code, StatusCode::OK);
    let succeeded_at = w.now.lock().unwrap().as_second().to_string();

    w.dog.reset().await;
    Mock::given(method("POST"))
        .and(path("/ping/x"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&w.dog)
        .await;
    // Still fresh (limit 300 s): the ping is forwarded, and refused.
    w.advance(10);
    let (code, _) = w
        .call(Request::post("/watchdog").body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert_eq!(w.dog.received_requests().await.unwrap().len(), 1);
    let m = scrape(&w).await;
    assert_eq!(metric(&m, "insist_watchdog_forward_failures_total"), "1");
    assert_eq!(
        metric(&m, "insist_watchdog_last_success_timestamp_seconds"),
        succeeded_at
    );
}

#[tokio::test]
async fn an_unreachable_dead_mans_switch_is_counted_and_never_named() {
    // A port that was just free: nothing listens, the connection is refused.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let w = World::with_watchdog_url(200, Some(format!("http://{closed}/ping/secret-path"))).await;
    good_reconcile(&w).await;
    let (code, body) = w
        .call(Request::post("/watchdog").body(Body::from("{}")).unwrap())
        .await;
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert!(!body.contains("secret-path"), "{body}");
    let m = scrape(&w).await;
    assert_eq!(metric(&m, "insist_watchdog_forward_failures_total"), "1");
    assert_eq!(
        metric(&m, "insist_watchdog_last_success_timestamp_seconds"),
        "0"
    );
}
