//! Alertmanager's wire format, as recorded in fixtures/alertmanager-0.31.1.
//! Only the fields this program reads are declared.
use jiff::Timestamp;
use serde::Deserialize;
use std::collections::BTreeMap;

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
    pub fingerprint: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GettableAlert {
    pub fingerprint: String,
    pub starts_at: Timestamp,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub annotations: Labels,
    pub status: AlertStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertStatus {
    pub state: String,
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
        let suppressed: Vec<GettableAlert> =
            serde_json::from_str(&fixture("api-suppressed.json")).unwrap();
        assert!(suppressed.iter().any(|a| a.status.state == "suppressed"));
        let empty: Vec<GettableAlert> = serde_json::from_str(&fixture("api-empty.json")).unwrap();
        assert!(empty.is_empty());
    }
}
