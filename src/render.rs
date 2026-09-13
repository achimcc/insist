//! What a notification says. Ported from the homeserver's alarm-uebersetzer.
use crate::alertmanager::Labels;
use crate::config::Texts;
use jiff::{tz::TimeZone, Timestamp};

#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub title: String,
    pub message: String,
    pub priority: u8,
    pub tags: Vec<String>,
}

pub fn alertname(labels: &Labels) -> String {
    labels
        .get("alertname")
        .cloned()
        .unwrap_or_else(|| "Alert".into())
}

/// The sentence the rule wrote itself; failing that, the label set.
pub fn summary(labels: &Labels, annotations: &Labels) -> String {
    match annotations.get("summary") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => labels
            .iter()
            .filter(|(k, _)| k.as_str() != "alertname")
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

pub fn clock(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone()).strftime("%H:%M").to_string()
}

pub fn render_alert(
    labels: &Labels,
    annotations: &Labels,
    starts_at: Timestamp,
    resolved: bool,
    texts: &Texts,
    tz: &TimeZone,
) -> Rendered {
    let name = alertname(labels);
    if resolved {
        return Rendered {
            title: format!("{}: {name}", texts.resolved),
            message: summary(labels, annotations),
            priority: 2,
            tags: vec!["white_check_mark".into()],
        };
    }
    let critical = labels.get("severity").map(String::as_str) == Some("critical");
    Rendered {
        title: name,
        message: format!(
            "{} ({} {})",
            summary(labels, annotations),
            texts.since,
            clock(starts_at, tz)
        ),
        priority: if critical { 5 } else { 4 },
        tags: vec![if critical {
            "rotating_light"
        } else {
            "warning"
        }
        .into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alertmanager::WebhookMessage;
    use crate::config::Texts;

    fn berlin() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("Europe/Berlin").unwrap()
    }

    fn single() -> WebhookMessage {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/alertmanager-0.31.1/webhook-firing-single.json"
        ))
        .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn a_critical_alert_is_urgent_and_says_what_and_since_when() {
        let a = &single().alerts[0];
        let r = render_alert(
            &a.labels,
            &a.annotations,
            a.starts_at,
            false,
            &Texts::default(),
            &berlin(),
        );
        assert_eq!(r.title, "UnitFehlgeschlagen");
        assert_eq!(r.priority, 5);
        assert_eq!(r.tags, vec!["rotating_light".to_string()]);
        assert!(
            r.message
                .starts_with("Unit lan6-set.service auf server ist rot"),
            "{}",
            r.message
        );
        assert!(r.message.contains("since "), "{}", r.message);
    }

    #[test]
    fn without_severity_it_is_a_warning_and_keeps_umlauts() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/alertmanager-0.31.1/webhook-no-severity.json"
        ))
        .unwrap();
        let m: WebhookMessage = serde_json::from_str(&raw).unwrap();
        let a = &m.alerts[0];
        let r = render_alert(
            &a.labels,
            &a.annotations,
            a.starts_at,
            false,
            &Texts::default(),
            &berlin(),
        );
        assert_eq!(r.priority, 4);
        assert!(r.message.contains("Prüfung — ä"));
    }

    #[test]
    fn a_resolved_alert_is_low_and_prefixed() {
        let a = &single().alerts[0];
        let r = render_alert(
            &a.labels,
            &a.annotations,
            a.starts_at,
            true,
            &Texts::default(),
            &berlin(),
        );
        assert_eq!(r.title, "Resolved: UnitFehlgeschlagen");
        assert_eq!(r.priority, 2);
    }

    #[test]
    fn without_a_summary_the_labels_stand_in_sorted_and_without_alertname() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("alertname".to_string(), "X".to_string());
        labels.insert("b".to_string(), "2".to_string());
        labels.insert("a".to_string(), "1".to_string());
        assert_eq!(summary(&labels, &Default::default()), "a=1, b=2");
    }
}
