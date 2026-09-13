use insist::alertmanager::{AlertmanagerClient, ApiError, PostableAlert};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn recorded(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/fixtures/alertmanager-0.31.1/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn answering(code: u16, body: &str) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/alerts"))
        .respond_with(ResponseTemplate::new(code).set_body_string(body))
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn reads_the_recorded_active_list() {
    let s = answering(200, &recorded("api-active.json")).await;
    let alerts = AlertmanagerClient::new(&s.uri())
        .unwrap()
        .alerts()
        .await
        .unwrap();
    assert_eq!(alerts[0].status.state, "active");
}

#[tokio::test]
async fn an_empty_list_is_a_valid_answer() {
    let s = answering(200, &recorded("api-empty.json")).await;
    assert!(AlertmanagerClient::new(&s.uri())
        .unwrap()
        .alerts()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_error_is_never_an_empty_list() {
    let s = answering(500, "boom").await;
    assert!(matches!(
        AlertmanagerClient::new(&s.uri()).unwrap().alerts().await,
        Err(ApiError::Status(500))
    ));
    let s = answering(200, "{\"not\":\"a list\"}").await;
    assert!(matches!(
        AlertmanagerClient::new(&s.uri()).unwrap().alerts().await,
        Err(ApiError::Malformed)
    ));
    let s = answering(200, "").await;
    assert!(matches!(
        AlertmanagerClient::new(&s.uri()).unwrap().alerts().await,
        Err(ApiError::Malformed)
    ));
}

#[tokio::test]
async fn nobody_listening_is_a_transport_error() {
    let c = AlertmanagerClient::new("http://127.0.0.1:9").unwrap();
    assert!(matches!(c.alerts().await, Err(ApiError::Transport)));
}

#[tokio::test]
async fn posts_alerts_in_the_shape_the_api_expects() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/alerts"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("alertname".to_string(), "AlarmUnquittiert".to_string());
    let alert = PostableAlert {
        labels,
        annotations: Default::default(),
        ends_at: Some("2026-09-13T13:00:00Z".parse().unwrap()),
    };
    AlertmanagerClient::new(&s.uri())
        .unwrap()
        .post(&[alert])
        .await
        .unwrap();
    let body = String::from_utf8(s.received_requests().await.unwrap()[0].body.clone()).unwrap();
    // raw, not re-parsed by our own types: the field name is the API's
    assert!(body.starts_with('['), "{body}");
    assert!(
        body.contains("\"endsAt\":\"2026-09-13T13:00:00Z\""),
        "{body}"
    );
    assert!(
        body.contains("\"alertname\":\"AlarmUnquittiert\""),
        "{body}"
    );
}
