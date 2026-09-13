//! Prometheus text format, written by hand: eight numbers do not need a crate.
use jiff::Timestamp;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

/// Shared independently of the runtime's own lock: `/metrics` reads this
/// directly, so a Prometheus scrape never waits behind a slow send to ntfy.
pub type MetricsHandle = Arc<Mutex<Metrics>>;

#[derive(Debug, Default)]
pub struct Metrics {
    pub ack_rejected_total: u64,
    pub publish_failures_total: u64,
    /// Sends that were due in the last pass and failed. The alert
    /// InsistSendetNicht fires on this staying above zero.
    pub publish_pending: u64,
    pub state_corrupt_total: u64,
    pub state_save_failures_total: u64,
    /// Attempts to raise the "unacknowledged" mail alert in Alertmanager
    /// that Alertmanager refused. A failed raise otherwise shows up only in
    /// the journal, and the alert it stands in for never fires.
    pub raise_failures_total: u64,
    pub last_reconcile_success: Option<Timestamp>,
    pub last_publish_success: Option<Timestamp>,
    pub open_instances: u64,
}

impl Metrics {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |name: &str, kind: &str, help: &str, value: String| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
            );
        };
        line(
            "insist_ack_rejected_total",
            "counter",
            "Acknowledgement bodies that failed verification.",
            self.ack_rejected_total.to_string(),
        );
        line(
            "insist_publish_failures_total",
            "counter",
            "Notifications ntfy did not accept.",
            self.publish_failures_total.to_string(),
        );
        line(
            "insist_publish_pending",
            "gauge",
            "Notifications due in the last pass that failed.",
            self.publish_pending.to_string(),
        );
        line(
            "insist_state_corrupt_total",
            "counter",
            "State files moved aside as unreadable.",
            self.state_corrupt_total.to_string(),
        );
        line(
            "insist_state_save_failures_total",
            "counter",
            "Failed writes of the state file.",
            self.state_save_failures_total.to_string(),
        );
        line(
            "insist_raise_failures_total",
            "counter",
            "Attempts to raise the unacknowledged alert that Alertmanager refused.",
            self.raise_failures_total.to_string(),
        );
        line(
            "insist_open_instances",
            "gauge",
            "Instances neither acknowledged nor resolved.",
            self.open_instances.to_string(),
        );
        let seconds = |t: Option<Timestamp>| t.map(|t| t.as_second()).unwrap_or(0).to_string();
        line(
            "insist_last_reconcile_success_timestamp_seconds",
            "gauge",
            "Last successful read of Alertmanager's alerts.",
            seconds(self.last_reconcile_success),
        );
        line(
            "insist_last_publish_success_timestamp_seconds",
            "gauge",
            "Last notification ntfy accepted.",
            seconds(self.last_publish_success),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_every_counter_and_gauge_including_raise_failures() {
        let m = Metrics {
            raise_failures_total: 3,
            ..Metrics::default()
        };
        let out = m.render();
        for name in [
            "insist_ack_rejected_total",
            "insist_publish_failures_total",
            "insist_publish_pending",
            "insist_state_corrupt_total",
            "insist_state_save_failures_total",
            "insist_raise_failures_total",
            "insist_open_instances",
            "insist_last_reconcile_success_timestamp_seconds",
            "insist_last_publish_success_timestamp_seconds",
        ] {
            assert!(out.contains(name), "{name} missing from:\n{out}");
        }
        assert!(out.contains("insist_raise_failures_total 3"), "{out}");
    }
}
