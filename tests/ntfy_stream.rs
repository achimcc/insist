use insist::metrics::MetricsHandle;
use insist::ntfy::{NtfyClient, StreamError};
use insist::secret::Secret;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn recorded(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/fixtures/ntfy-2.26.0/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn seen() -> MetricsHandle {
    MetricsHandle::default()
}

fn topic() -> Secret {
    Secret::from("alarmtopic-quittung".to_string())
}

#[tokio::test]
async fn delivers_only_message_events_from_a_recorded_stream() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alarmtopic/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(recorded("stream.ndjson")))
        .mount(&s)
        .await;
    let c = NtfyClient::new(&s.uri(), Secret::from("tk".to_string())).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.read_acks(
        &Secret::from("alarmtopic".to_string()),
        None,
        Duration::from_secs(5),
        &tx,
        &seen(),
    )
    .await
    .unwrap();
    drop(tx);
    let mut events = vec![];
    while let Some(line) = rx.recv().await {
        events.push(line.event);
    }
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| e == "message"));
}

#[tokio::test]
async fn reads_the_recorded_button_bodies_with_the_token_and_since() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alarmtopic-quittung/json"))
        .and(header("authorization", "Bearer tk_alarm"))
        .respond_with(ResponseTemplate::new(200).set_body_string(recorded("ack-poll.ndjson")))
        .mount(&s)
        .await;
    let c = NtfyClient::new(&s.uri(), Secret::from("tk_alarm".to_string())).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.read_acks(
        &topic(),
        Some("hwQ2YpKdmg"),
        Duration::from_secs(5),
        &tx,
        &seen(),
    )
    .await
    .unwrap();
    drop(tx);
    let first = rx.recv().await.unwrap();
    assert_eq!(
        first.message.as_deref(),
        Some("a1.0123456789abcdef.00000000000000000000000000000000")
    );
    // raw query, not decoded by a matcher (the signal-seerr lesson)
    let url = s.received_requests().await.unwrap()[0].url.clone();
    assert_eq!(url.query(), Some("since=hwQ2YpKdmg"));
}

#[tokio::test]
async fn without_a_cursor_it_reads_everything_ntfy_still_holds() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&s)
        .await;
    let c = NtfyClient::new(&s.uri(), Secret::from("tk".to_string())).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    c.read_acks(&topic(), None, Duration::from_secs(5), &tx, &seen())
        .await
        .unwrap();
    assert_eq!(
        s.received_requests().await.unwrap()[0].url.query(),
        Some("since=all")
    );
}

#[tokio::test]
async fn a_403_and_a_silent_server_are_errors() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&s)
        .await;
    let c = NtfyClient::new(&s.uri(), Secret::from("tk".to_string())).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let refused = seen();
    assert!(matches!(
        c.read_acks(&topic(), None, Duration::from_secs(5), &tx, &refused)
            .await,
        Err(StreamError::Status(403))
    ));
    assert!(refused.lock().unwrap().ack_stream_last_event.is_none());

    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&s)
        .await;
    let c = NtfyClient::new(&s.uri(), Secret::from("tk".to_string())).unwrap();
    assert!(matches!(
        c.read_acks(&topic(), None, Duration::from_millis(300), &tx, &seen())
            .await,
        Err(StreamError::Idle)
    ));
}

#[tokio::test]
async fn every_line_moves_the_stream_gauge_keepalives_included() {
    // Only the keepalive lines of the recording (ntfy 2.26.0 with
    // keepalive-interval 5s): no message at all, and still proof of life.
    let keepalives: String = recorded("stream.ndjson")
        .lines()
        .filter(|l| l.contains("\"event\":\"keepalive\""))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(keepalives.lines().count(), 2, "{keepalives}");
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(keepalives))
        .mount(&s)
        .await;
    let c = NtfyClient::new(&s.uri(), Secret::from("tk".to_string())).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let metrics = seen();
    let before = jiff::Timestamp::now().as_second();
    c.read_acks(&topic(), None, Duration::from_secs(5), &tx, &metrics)
        .await
        .unwrap();
    let after = jiff::Timestamp::now().as_second();
    drop(tx);
    assert!(rx.recv().await.is_none(), "a keepalive is not a message");
    let last = metrics
        .lock()
        .unwrap()
        .ack_stream_last_event
        .expect("a keepalive must set the gauge")
        .as_second();
    assert!(
        before <= last && last <= after,
        "{before} <= {last} <= {after}"
    );
    let rendered = metrics.lock().unwrap().render();
    assert!(
        rendered.contains(&format!(
            "\ninsist_ack_stream_last_event_timestamp_seconds {last}\n"
        )),
        "{rendered}"
    );
}
