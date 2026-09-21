//! Alertmanager's wire format, as recorded in fixtures/alertmanager-0.31.1.
//! Only the fields this program reads are declared.
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

pub type Labels = BTreeMap<String, String>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookMessage {
    pub status: String,
    pub alerts: Vec<WebhookAlert>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookAlert {
    pub status: String,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub annotations: Labels,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GettableAlert {
    pub fingerprint: String,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub annotations: Labels,
    pub status: AlertStatus,
    pub receivers: Vec<Receiver>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertStatus {
    pub state: String,
}

/// One of Alertmanager's configured receivers, as named on an alert it
/// routed. Only the name is read: it is what `Config::receivers` compares
/// against to tell "this alert reached insist's webhook" from "this alert
/// merely exists and is routed to mail only".
#[derive(Debug, Clone, Deserialize)]
pub struct Receiver {
    pub name: String,
}

/// A silence as `GET /api/v2/silences` lists it (recorded in
/// `silences-*.json`). Expired silences stay in that list, so only
/// `status.state == "active"` means one is muting something right now.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GettableSilence {
    pub id: String,
    pub status: SilenceStatus,
    pub matchers: Vec<Matcher>,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub comment: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SilenceStatus {
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Matcher {
    pub name: String,
    pub value: String,
    pub is_regex: bool,
    /// Absent in answers from before Alertmanager knew negative matchers;
    /// absent means equal.
    #[serde(default = "yes")]
    pub is_equal: bool,
}

fn yes() -> bool {
    true
}

impl GettableSilence {
    pub fn is_active(&self) -> bool {
        self.status.state == "active"
    }

    /// The matchers the way Alertmanager's own UI writes them:
    /// `alertname=~".+", instance!="server"`.
    pub fn matchers_text(&self) -> String {
        self.matchers
            .iter()
            .map(|m| {
                let op = match (m.is_equal, m.is_regex) {
                    (true, false) => "=",
                    (false, false) => "!=",
                    (true, true) => "=~",
                    (false, true) => "!~",
                };
                format!("{}{op}\"{}\"", m.name, m.value)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Alertmanager writes exactly this value into `endsAt` for a firing alert
/// that has no resolve timeout yet (every recorded firing webhook carries
/// it); it is not a real deadline.
const UNSET_ENDS_AT: &str = "0001-01-01T00:00:00Z";

/// `endsAt` as insist understands it: Alertmanager's zero value becomes
/// `None` (unknown, never expires by itself), everything else is a real
/// deadline reconciliation can compare `now` against.
pub fn known_ends_at(ends_at: Timestamp) -> Option<Timestamp> {
    let unset: Timestamp = UNSET_ENDS_AT.parse().expect("valid constant timestamp");
    (ends_at != unset).then_some(ends_at)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostableAlert {
    pub labels: Labels,
    pub annotations: Labels,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<Timestamp>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Alertmanager answered HTTP {0}")]
    Status(u16),
    #[error("Alertmanager's answer is not the list it should be")]
    Malformed,
    #[error("Alertmanager could not be reached")]
    Transport,
}

pub struct AlertmanagerClient {
    http: reqwest::Client,
    base: String,
}

impl AlertmanagerClient {
    pub fn new(base: &str) -> anyhow::Result<AlertmanagerClient> {
        Ok(AlertmanagerClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// Only a 200 with a JSON array counts. Anything else is an error, and an
    /// error must never be read as "nothing is firing".
    pub async fn alerts(&self) -> Result<Vec<GettableAlert>, ApiError> {
        let answer = self
            .http
            .get(format!("{}/api/v2/alerts", self.base))
            .send()
            .await
            .map_err(|_| ApiError::Transport)?;
        if answer.status().as_u16() != 200 {
            return Err(ApiError::Status(answer.status().as_u16()));
        }
        let bytes = answer.bytes().await.map_err(|_| ApiError::Transport)?;
        serde_json::from_slice(&bytes).map_err(|_| ApiError::Malformed)
    }

    /// Same rule as `alerts`: only a 200 with a JSON array is an answer. An
    /// error is never "no silence is active".
    pub async fn silences(&self) -> Result<Vec<GettableSilence>, ApiError> {
        let answer = self
            .http
            .get(format!("{}/api/v2/silences", self.base))
            .send()
            .await
            .map_err(|_| ApiError::Transport)?;
        if answer.status().as_u16() != 200 {
            return Err(ApiError::Status(answer.status().as_u16()));
        }
        let bytes = answer.bytes().await.map_err(|_| ApiError::Transport)?;
        serde_json::from_slice(&bytes).map_err(|_| ApiError::Malformed)
    }

    pub async fn post(&self, alerts: &[PostableAlert]) -> Result<(), ApiError> {
        let answer = self
            .http
            .post(format!("{}/api/v2/alerts", self.base))
            .json(alerts)
            .send()
            .await
            .map_err(|_| ApiError::Transport)?;
        if answer.status().is_success() {
            Ok(())
        } else {
            Err(ApiError::Status(answer.status().as_u16()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/fixtures/alertmanager-0.31.1/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn reads_a_recorded_firing_webhook() {
        let m: WebhookMessage =
            serde_json::from_str(&fixture("webhook-firing-single.json")).unwrap();
        assert_eq!(m.status, "firing");
        assert_eq!(m.alerts.len(), 1);
        let a = &m.alerts[0];
        assert_eq!(a.labels["name"], "lan6-set.service");
        assert_eq!(a.fingerprint.len(), 16);
        assert!(a.annotations["summary"].contains("lan6-set"));
        assert_eq!(
            known_ends_at(a.ends_at),
            None,
            "a firing alert's endsAt is the zero value"
        );
    }

    #[test]
    fn a_resolved_webhook_carries_a_real_ends_at() {
        let r: WebhookMessage = serde_json::from_str(&fixture("webhook-resolved.json")).unwrap();
        assert!(known_ends_at(r.alerts[0].ends_at).is_some());
    }

    #[test]
    fn reads_a_recorded_group_and_a_resolved_webhook() {
        let g: WebhookMessage =
            serde_json::from_str(&fixture("webhook-firing-group.json")).unwrap();
        assert_eq!(g.alerts.len(), 2);
        let r: WebhookMessage = serde_json::from_str(&fixture("webhook-resolved.json")).unwrap();
        assert!(r.alerts.iter().any(|a| a.status == "resolved"));
    }

    #[test]
    fn reads_the_recorded_api_answers() {
        let active: Vec<GettableAlert> = serde_json::from_str(&fixture("api-active.json")).unwrap();
        assert_eq!(active[0].status.state, "active");
        assert_eq!(active[0].receivers[0].name, "rec");
        let suppressed: Vec<GettableAlert> =
            serde_json::from_str(&fixture("api-suppressed.json")).unwrap();
        assert!(suppressed.iter().any(|a| a.status.state == "suppressed"));
        let empty: Vec<GettableAlert> = serde_json::from_str(&fixture("api-empty.json")).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn reads_the_recorded_silences_and_counts_only_active_ones() {
        let empty: Vec<GettableSilence> =
            serde_json::from_str(&fixture("silences-empty.json")).unwrap();
        assert!(empty.is_empty());

        let both: Vec<GettableSilence> =
            serde_json::from_str(&fixture("silences-active-and-pending.json")).unwrap();
        let active: Vec<_> = both.iter().filter(|s| s.is_active()).collect();
        assert_eq!(active.len(), 1, "one active, one pending");
        assert_eq!(active[0].comment, "everything");
        assert_eq!(active[0].created_by, "record-silences.sh");
        assert_eq!(active[0].matchers_text(), r#"alertname=~".+""#);
        assert_eq!(active[0].id.len(), 36);

        let pending = both.iter().find(|s| !s.is_active()).unwrap();
        assert_eq!(
            pending.matchers_text(),
            r#"alertname="UnitFehlgeschlagen", instance!="server""#
        );

        // An expired silence stays in the list; it must not count.
        let after: Vec<GettableSilence> =
            serde_json::from_str(&fixture("silences-after-expire.json")).unwrap();
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|s| !s.is_active()));
    }

    #[test]
    fn known_ends_at_maps_the_zero_value_to_none_and_keeps_everything_else() {
        let unset: Timestamp = "0001-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(known_ends_at(unset), None);
        let real: Timestamp = "2026-09-13T12:36:04.000Z".parse().unwrap();
        assert_eq!(known_ends_at(real), Some(real));
    }
}
