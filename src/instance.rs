//! One firing of one alert: Alertmanager's fingerprint plus its startsAt.
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceId(String);

impl InstanceId {
    /// Truncated to milliseconds: the webhook carries nanoseconds, the API
    /// milliseconds. Without the truncation one instance would get two ids,
    /// and reconciliation would take the webhook's for resolved.
    pub fn of(fingerprint: &str, starts_at: Timestamp) -> InstanceId {
        let digest = Sha256::digest(format!("{fingerprint}|{}", starts_at.as_millisecond()));
        InstanceId(hex::encode(&digest[..8]))
    }

    pub fn parse(s: &str) -> Option<InstanceId> {
        let valid = s.len() == 16
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        valid.then(|| InstanceId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for InstanceId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        InstanceId::parse(&s).ok_or_else(|| "not an instance id".to_string())
    }
}

impl From<InstanceId> for String {
    fn from(id: InstanceId) -> String {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceId;
    use crate::alertmanager::{GettableAlert, WebhookMessage};

    fn recorded(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/fixtures/alertmanager-0.31.1/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn webhook_and_api_name_the_same_instance_alike() {
        // The reason the id truncates to milliseconds: the webhook and the
        // API serialise startsAt with different precision (fixtures SOURCE.md).
        let w: WebhookMessage =
            serde_json::from_str(&recorded("webhook-firing-single.json")).unwrap();
        let a: Vec<GettableAlert> = serde_json::from_str(&recorded("api-active.json")).unwrap();
        let from_webhook = InstanceId::of(&w.alerts[0].fingerprint, w.alerts[0].starts_at);
        let api = a
            .iter()
            .find(|x| x.fingerprint == w.alerts[0].fingerprint)
            .unwrap();
        assert_eq!(
            from_webhook,
            InstanceId::of(&api.fingerprint, api.starts_at)
        );
    }

    #[test]
    fn a_new_start_is_a_new_instance() {
        let t: jiff::Timestamp = "2026-09-11T15:55:07.123Z".parse().unwrap();
        let later: jiff::Timestamp = "2026-09-11T18:00:00Z".parse().unwrap();
        assert_ne!(
            InstanceId::of("abcdef0123456789", t),
            InstanceId::of("abcdef0123456789", later)
        );
    }

    #[test]
    fn nanoseconds_within_one_millisecond_do_not_matter() {
        let a: jiff::Timestamp = "2026-09-11T15:55:07.123000001Z".parse().unwrap();
        let b: jiff::Timestamp = "2026-09-11T15:55:07.123999999Z".parse().unwrap();
        assert_eq!(InstanceId::of("f", a), InstanceId::of("f", b));
    }

    #[test]
    fn parse_accepts_only_sixteen_lowercase_hex() {
        assert!(InstanceId::parse("0123456789abcdef").is_some());
        assert!(InstanceId::parse("0123456789ABCDEF").is_none());
        assert!(InstanceId::parse("0123456789abcde").is_none());
        assert!(InstanceId::parse("0123456789abcdeg").is_none());
    }
}
