//! What a notification says. Ported from the homeserver's alarm-uebersetzer.
use crate::alertmanager::Labels;
use jiff::{tz::TimeZone, Timestamp};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("Europe/Berlin").unwrap()
    }

    #[test]
    fn clock_converts_to_local_time_with_dst() {
        let tz = berlin();

        // Summer time (UTC+2): 2026-09-13T12:36:04Z → 14:36 Berlin
        let summer = jiff::Timestamp::from_second(1789302964).unwrap();
        assert_eq!(clock(summer, &tz), "14:36");

        // Winter time (UTC+1): 2026-12-01T12:36:04Z → 13:36 Berlin
        let winter = jiff::Timestamp::from_second(1796128564).unwrap();
        assert_eq!(clock(winter, &tz), "13:36");
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
