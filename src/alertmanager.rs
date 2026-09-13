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
    fn known_ends_at_maps_the_zero_value_to_none_and_keeps_everything_else() {
        let unset: Timestamp = "0001-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(known_ends_at(unset), None);
        let real: Timestamp = "2026-09-13T12:36:04.000Z".parse().unwrap();
        assert_eq!(known_ends_at(real), Some(real));
    }
}
