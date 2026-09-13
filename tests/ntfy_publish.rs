use insist::ntfy::{NtfyClient, Publication, PublishError};
use insist::secret::Secret;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn publication() -> Publication {
    Publication {
        topic: Secret::from("alarmtopic".to_string()),
        title: "Prüfung — ä".into(),
        message: "m".into(),
        priority: 4,
        tags: vec![],
        sequence_id: Some("0123456789abcdef".into()),
        actions: vec![],
    }
}

fn recorded(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/fixtures/ntfy-2.26.0/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[tokio::test]
async fn publishes_json_to_the_root_with_the_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", "Bearer tk_alarm"))
        .respond_with(ResponseTemplate::new(200).set_body_string(recorded("publish-response.json")))
        .expect(1)
        .mount(&server)
        .await;
    let client = NtfyClient::new(&server.uri(), Secret::from("tk_alarm".to_string())).unwrap();
    client.publish(&publication()).await.unwrap();

    // Checked raw, not through our own serializer: the title must arrive as
    // UTF-8 inside the body, not as a header.
    let requests = server.received_requests().await.unwrap();
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("Prüfung — ä"), "{body}");
    assert!(requests[0].headers.get("title").is_none());
}

#[tokio::test]
async fn a_403_is_an_error_that_names_the_status_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("{\"code\":40301,\"error\":\"forbidden\"}"),
        )
        .mount(&server)
        .await;
    let client = NtfyClient::new(&server.uri(), Secret::from("tk_alarm".to_string())).unwrap();
    let err = client.publish(&publication()).await.unwrap_err();
    assert!(matches!(err, PublishError::Status(403)));
    assert!(!err.to_string().contains("alarmtopic"));
}
