//! Publishing to ntfy. Shapes follow fixtures/ntfy-2.26.0.
use crate::metrics::MetricsHandle;
use crate::secret::Secret;
use serde_json::{json, Map, Value};
use std::time::Duration;

/// One notification. Deliberately no Debug: the topic name and the button's
/// token are secrets.
pub struct Publication {
    pub topic: Secret,
    pub title: String,
    pub message: String,
    pub priority: u8,
    pub tags: Vec<String>,
    /// Same id → the phone replaces the earlier notification.
    pub sequence_id: Option<String>,
    pub actions: Vec<Action>,
}

/// An http button the phone executes itself.
pub struct Action {
    pub label: String,
    pub url: Secret,
    pub token: Secret,
    pub body: String,
}

impl Publication {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("topic".into(), json!(self.topic.expose()));
        m.insert("title".into(), json!(self.title));
        m.insert("message".into(), json!(self.message));
        m.insert("priority".into(), json!(self.priority));
        if !self.tags.is_empty() {
            m.insert("tags".into(), json!(self.tags));
        }
        if let Some(id) = &self.sequence_id {
            m.insert("sequence_id".into(), json!(id));
        }
        if !self.actions.is_empty() {
            let actions: Vec<Value> = self
                .actions
                .iter()
                .map(|a| {
                    json!({
                        "action": "http",
                        "label": a.label,
                        "url": a.url.expose(),
                        "method": "POST",
                        "headers": { "Authorization": format!("Bearer {}", a.token.expose()) },
                        "body": a.body,
                        "clear": true,
                    })
                })
                .collect();
            m.insert("actions".into(), Value::Array(actions));
        }
        Value::Object(m)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("ntfy answered HTTP {0}")]
    Status(u16),
    /// The reqwest error is not kept: its Display can contain the URL.
    #[error("ntfy could not be reached")]
    Transport,
}

pub struct NtfyClient {
    http: reqwest::Client,
    stream_http: reqwest::Client,
    base: String,
    token: Secret,
}

impl NtfyClient {
    pub fn new(base: &str, token: Secret) -> anyhow::Result<NtfyClient> {
        Ok(NtfyClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            stream_http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn publish(&self, p: &Publication) -> Result<(), PublishError> {
        let answer = self
            .http
            .post(format!("{}/", self.base))
            .bearer_auth(self.token.expose())
            .json(&p.to_json())
            .send()
            .await
            .map_err(|_| PublishError::Transport)?;
        if answer.status().is_success() {
            Ok(())
        } else {
            Err(PublishError::Status(answer.status().as_u16()))
        }
    }

    /// Reads `<topic>/json` from `since` (or everything ntfy still caches) and
    /// hands every message event to `tx`. Returns Ok when ntfy closes the
    /// connection; the caller reconnects. `idle` must exceed ntfy's keepalive
    /// interval (45 s by default) — silence longer than that is a dead link.
    ///
    /// Every event line ntfy sends, keepalives included, moves `seen`'s
    /// `ack_stream_last_event`: a stream that stopped reading shows as a
    /// gauge that stopped moving, not only as a journal line.
    pub async fn read_acks(
        &self,
        topic: &Secret,
        since: Option<&str>,
        idle: Duration,
        tx: &tokio::sync::mpsc::UnboundedSender<StreamLine>,
        seen: &MetricsHandle,
    ) -> Result<(), StreamError> {
        let request = self
            .stream_http
            .get(format!("{}/{}/json", self.base, topic.expose()))
            .query(&[("since", since.unwrap_or("all"))])
            .bearer_auth(self.token.expose())
            .send();
        let mut answer = tokio::time::timeout(idle, request)
            .await
            .map_err(|_| StreamError::Idle)?
            .map_err(|_| StreamError::Transport)?;
        if !answer.status().is_success() {
            return Err(StreamError::Status(answer.status().as_u16()));
        }
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let chunk = tokio::time::timeout(idle, answer.chunk())
                .await
                .map_err(|_| StreamError::Idle)?
                .map_err(|_| StreamError::Transport)?;
            let Some(chunk) = chunk else { break };
            buffer.extend_from_slice(&chunk);
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                deliver(&line, tx, seen);
            }
        }
        deliver(&buffer, tx, seen);
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct StreamLine {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("ntfy answered HTTP {0}")]
    Status(u16),
    #[error("ntfy sent nothing, not even a keepalive, within the idle limit")]
    Idle,
    #[error("ntfy could not be reached")]
    Transport,
}

fn deliver(line: &[u8], tx: &tokio::sync::mpsc::UnboundedSender<StreamLine>, seen: &MetricsHandle) {
    if let Ok(parsed) = serde_json::from_slice::<StreamLine>(line) {
        seen.lock().unwrap().ack_stream_last_event = Some(jiff::Timestamp::now());
        if parsed.event == "message" {
            let _ = tx.send(parsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::Secret;

    #[test]
    fn the_json_carries_topic_sequence_and_a_post_action() {
        let p = Publication {
            topic: Secret::from("alarmtopic".to_string()),
            title: "T".into(),
            message: "M".into(),
            priority: 5,
            tags: vec!["rotating_light".into()],
            sequence_id: Some("0123456789abcdef".into()),
            actions: vec![Action {
                label: "Quittieren".into(),
                url: Secret::from("https://ntfy.example/alarmtopic-quittung".to_string()),
                token: Secret::from("tk_knopf".to_string()),
                body: "a1.0123456789abcdef.00".into(),
            }],
        };
        let j = p.to_json();
        assert_eq!(j["topic"], "alarmtopic");
        assert_eq!(j["sequence_id"], "0123456789abcdef");
        assert_eq!(j["priority"], 5);
        let a = &j["actions"][0];
        assert_eq!(a["action"], "http");
        assert_eq!(a["method"], "POST");
        assert_eq!(a["headers"]["Authorization"], "Bearer tk_knopf");
        assert_eq!(a["body"], "a1.0123456789abcdef.00");
        assert_eq!(a["clear"], true);
    }

    #[test]
    fn empty_optional_fields_are_left_out() {
        let p = Publication {
            topic: Secret::from("t".to_string()),
            title: "T".into(),
            message: "M".into(),
            priority: 2,
            tags: vec![],
            sequence_id: None,
            actions: vec![],
        };
        let j = p.to_json();
        assert!(j.get("sequence_id").is_none());
        assert!(j.get("actions").is_none());
        assert!(j.get("tags").is_none());
    }
}
